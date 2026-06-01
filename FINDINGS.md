# Code Review — Rich Parsers

Review of the "rich-context" query system merged from the remote (PR #2):

| File | Role |
|---|---|
| `src/rich_context.rs` | Builds the structured `FunctionEntry` records (disasm ⋈ source statements ⋈ locals); writes the tree + `index.jsonl`. |
| `src/disasm.rs` | iced-x86 decode + symbol-resolving Intel formatter. |
| `src/rich_query.rs` | Streams `index.jsonl`, filters by name/RVA. |
| `src/rich_diff.rs` | Built-in LCS text diff of two instruction streams. |
| `src/rich_objdiff.rs` | objdiff-core operand-aware diff over delinker `.obj` files. |
| `src/rich_callees.rs` | Extracts + resolves call targets. |
| `src/rich_render.rs` | Renders a `FunctionEntry` into the listing/structure/info views. |
| `src/bin/pdb_rich_context.rs`, `pdb_rich_query.rs`, `pdb_fetch.rs` | CLIs. |

Reviewed at `master` = `af772f1`.

## Scope & method caveat
This is a **read-only** review. **No Rust toolchain is installed in this environment**
(`cargo`/`rustc`/`rustup` are all absent), so I could **not** compile, run `clippy`, or run the
tests — none of the findings below are backed by a build. The objdiff path mapping (M1) *was*
cross-checked against the sibling `vostok-delinker` source and the real on-disk
`vostok/binaries/objdiff/` tree. Tree is clean: no `.orig` files, no conflict markers.

**Overall:** the code is well-structured and unusually well-commented; the
"build structured data once, render/diff on top" split is clean and the offset-keyed source
interleave is a nice design. No bugs that break the build or core output; findings are one
efficiency win (M2) plus correctness/polish nits.

---

## Medium

### M1. `--view diff` objdiff path mapping is correct — but verify, then guard it (was a false alarm)
**Verified OK.** `pdb_fetch.rs:194-195` builds the object-file path as:
```rust
let bobj = bdir.join(format!("{}.obj", base.file));   // -> .../vostok/collision/sources/foo.cpp.obj
let tobj = tdir.join(format!("{}.obj", target.file));
```
`FunctionEntry.file` is the full per-source-file path (e.g. `vostok/collision/sources/box_geometry_instance.cpp`).
I initially suspected the delinker bucketed objects per *module folder* (`vostok/collision.obj`),
which would have made every lookup miss. **That was wrong.** Checked against the actual on-disk
config and objects in `vostok/binaries/objdiff/`:
```json
{ "name":        "vostok/collision/sources/box_geometry_instance.cpp",
  "target_path": "./target/vostok/collision/sources/box_geometry_instance.cpp.obj" }
```
and the real file exists at exactly
`binaries/objdiff/target/vostok/collision/sources/box_geometry_instance.cpp.obj`. The delinker
keys objects by the engine-relative *source path* (`object_files.rs` buckets on the per-symbol
`filename`, then writes `<dir>/<rel-path>.obj`), so `format!("{}.obj", base.file)` reproduces it
exactly. The `.cpp.obj` "double extension" is intentional and matches. No bug here.

Two genuine residual points:
- **Silent fallback hides real misses.** When the obj *is* absent (unbuilt unit, `/LTCG`-folded
  symbol, header-only `.h` unit), `rich_objdiff::diff` returns `Ok(None)` and `print_diff` drops to
  the text diff with only an `eprintln`. Since stdout still produces a plausible diff, a user can
  easily not notice they got the weaker backend. Consider making the note louder, or a `--strict`
  that errors instead of falling back.
- **No test pins the path mapping.** It's correct today but fragile (one stray `replace`/prefix
  change breaks it for every function at once). A single test resolving a known symbol against the
  real `binaries/objdiff/target` tree would lock it in.

(Process note: the Read/Bash tooling returned **garbled and duplicated output repeatedly** during
this review — duplicated `"name"` lines, swallowed `sed`/`find` output. An earlier draft of this
finding asserted a folder-level `vostok/collision.obj` that does not exist; I caught and corrected
it by reading the real obj filenames off disk. Mentioning so the rest of the file is read with that
in mind — the conclusions below were re-checked against source, not the garbled dumps.)

### M2. The PDB is opened and fully parsed three times per build
In `rich_context.rs:154-156`, `dump_rich_context` runs *inside*
`PdbParser::with(pdb_path, |fmt| { … })` and then independently does `File::open(pdb_path)` +
`PDB::open(file)`, parsing the whole PDB again. And `PdbParser::with` itself
(`pdb_parser.rs:31-44`) already opens + parses the PDB **twice** — once for `formatter`, once for
`formatter_orig` (its own doc comment flags this: *"this will keep 2 versions of the `pdb` file in
memory"*). So a single rich-context build parses the full game PDB **three times**. On top of that,
module symbols are walked twice within `dump_rich_context` — once in `build_symbol_maps`
(`:427-490`) and again in the main loop (`:170-292`).

That `2×` formatter cost is pre-existing infrastructure, not this PR's doing — but this PR's *own*
extra `PDB::open` (the 3rd parse) is new and avoidable. For a full game PDB it's a real
constant-factor cost. It's a one-shot "complete rebuild" so it's not hot; still, reusing the PDB
handle the formatter already holds (or at least folding the symbol-map pass into the main module
loop) is low-risk and roughly removes one full parse.

---

## Low / correctness nits

### L1. Unpaired-symbol fallback renders base instructions as the target
`rich_objdiff.rs:106-109`: when the base symbol has no paired `target_symbol`, the rows are built
with `make_row(l, l, origin)` — the base instruction is passed as **both** sides. `make_row` keys
the row off `l.kind`, so `None` rows come out as `RowKind::Equal` with `base == target`, implying a
match that was never computed against any target. The reported `match_percent` is honest here (the
strict `bsym.match_percent`), but the row view is misleading. Prefer rendering these as base-only
(`Delete`) or printing a "no target pairing" banner.

### L2. `rich_diff::diff` allocates a full O(n·m) LCS table
`rich_diff.rs:60` `let mut dp = vec![vec![0u32; m + 1]; n + 1];` — a 2-D table plus `n+1` separate
`Vec` allocations. Fine for typical functions, but a large LTCG function (thousands of instructions
per side) is tens of MB. A flattened `Vec<u32>` (or rolling two rows + Hirschberg for the
backtrack) would bound it. Watch-item, not urgent.

### L3. `callees` resolution rescans + re-parses the whole index, allocating per line
`rich_callees.rs:64-78`: streams and `serde_json`-parses every `index.jsonl` entry, and for each
line builds `format!("{callee}(")` for every callee — O(lines × callees) with an allocation per
inner iteration. Hoist the `format!("{callee}(")` patterns out of the line loop (compute once per
callee), and consider reusing an already-loaded index when an agent loop fetches many functions.

### L4. `parse_hex` is duplicated verbatim
Identical helper in `pdb_fetch.rs:69-75` and `pdb_rich_query.rs:38-44`. Move to a shared
`utils`/`lib` function.

### L5. View docs/UX drift — `info` and `callees` are undocumented
`pdb_fetch` dispatches `target|base|structure|info|callees|diff` (`:146-185`), but the `--view`
doc comment (`:54-55`) lists only `target, base, structure, diff`, and the unknown-view error
(`:185`) says `use target|base|structure|diff`. Both omit `info` and `callees`, so a user has no
discoverable way to learn those views exist.

### L6. `render_structure` prints `L0` for unknown source lines
`rich_render.rs:74` renders target statements with `line == 0` (no line info) as literal `L0`,
which reads as "line zero" rather than "unknown". Minor: `L?` or omit.

### L7. `index.jsonl` carries no version/identity stamp
The index is raw `FunctionEntry` JSON per line. The `#[serde(default)]` on
`label`/`source`/`locals` gives some forward-compat, but adding a non-defaulted field silently
breaks reading old indexes, and there's nothing tying an index to the PDB it was built from (an
index from a stale/other PDB queries with no warning). Low priority for a local tool; a one-line
header record (PDB age/guid + schema version) would make staleness detectable.

---

## Suggested order
1. **M2** — reuse the parsed PDB / single symbol pass (the one real efficiency win).
2. Correctness/UX: **L1** (misleading unpaired rows), **L5** (undocumented views), **M1**'s
   loud-fallback + path-mapping test.
3. Polish: **L3, L4, L2, L6, L7**.
