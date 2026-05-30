# WORK LOG — rich context & fetch

Running log of decisions **taken** and **not taken**, with rationale. See
`PLAN.md` for the design; this is the "why we chose X" history.

## Context

Worktree `rich-context-binary` (branch `worktree-rich-context-binary`). Goal: a
structured, queryable context layer for AI binary matching — disassembly paired
with source statements, served as views (target/base/structure/diff), built per
side from the PDB+EXE.

Inputs:
- target: `vcproj2ninja/survarium.{exe,pdb}` (also in the nix store), engine
  path `c:\survarium\sources`.
- base: `vostok/binaries/Win32/survarium-dx11-win32-gold.{exe,pdb}`, engine path
  `z:\home\sheep\projects\surv-decomp\vostok\sources\`, source root
  `vostok/sources`. (The base `.exe` is rebuilt; an early note that it was 0
  bytes is stale.)

Two reusable halves underpin everything, keyed by the **same RVA** so the merge
is exact, not heuristic:
- `gen_sources.rs` → per-function statements `(rva, line)` from the line program.
- `vostok-delinker/object_files.rs` → per-function `.text` bytes + iced-x86 walk.

## Decisions TAKEN

1. **Host in `vostok-pdb-parser`, copy the minimal delinker subset.** The richer
   source/type machinery lives here; the delinker half we need (decode loop +
   symbol RVA maps + section info) is small and was ported, not depended on.
   The delinker stays a working binary crate; we read its `.obj` outputs as
   files where needed (objdiff-core path).

2. **Structured data, not pre-rendered strings.** The index stores
   `FunctionEntry { name, rva, size, file, statements[], instructions[] }`; the
   tools render views from it. Required for the structure view and for diffing on
   raw asm. (Owner: "structured output, like objdiff, without rendering".)
   Originally we stored a rendered `block`; pivoted away from it.

3. **Per-instruction annotation is the instruction's SIZE, not an offset.**
   (Owner correction.) Offsets drift on any size change and aren't what matching
   compares. Sizes sum to the statement total (verified on `ghost_object`).

4. **Sizes render as hex with the word/anchor that disambiguates them.** Settled
   on `<0xNN>` for the statement size inline, after iterating decimal `N bytes` →
   `0xNN bytes` → the current `; <0xSIZE> ; <source>` inline form. Operands are
   hex, so the size is marked to not read as an address.

5. **Listing format** (matches the owner's `sum_range` example, which is "a
   slightly outdated display" so matched in spirit): offset-prefixed instructions
   `0xNN:  <asm>`, with `; <0xSIZE> ; <source>` appended only on each statement's
   first instruction; local labels on their own line. Target omits `<source>`.

6. **Statement with no source → still anchored by offset.** Target (always) and
   base inlined/headerless statements have no source text; the leading offset is
   their anchor. (Owner: "instead of source code we can specify address [0xFF]"
   then "also for target".) Line numbers are noise in the listing; kept in the
   structure view (they are part of "source structure").

7. **Synthetic local labels `.1/.2`** for in-function branch targets, operands
   rewritten to them; call/data targets named from the PDB symbol maps (module
   names win, public/mangled fill gaps).

8. **Query is served from a pre-built index, no per-query re-parse.** (Owner:
   "rebuild completely, then query on top"; "we can always optimize".) Full
   rebuild ~1.4 s; query over 24,467 functions ~0.13 s. Index is `index.jsonl`,
   sorted (file, rva) for stable diffs.

9. **`render_function` → `build_function`.** Returns the structured entry; the
   signature is computed once by the caller and both keys the index and heads the
   listing.

10. **Diff baseline = LCS over instruction text, computed before metadata.**
    Produces an Equal/Delete/Insert op stream + match ratio (the retry-budget
    signal). A matched function aligns near-100%; residual is label/symbol text
    noise (see "not taken / objdiff-core").

11. **Base↔target join by signature (`name`), not RVA.** RVAs differ between the
    two binaries; names don't. `pdb_fetch` resolves the target match first, then
    joins base by exact name.

12. **Three CLIs, separated by role.** `pdb_rich_context` (build),
    `pdb_rich_query` (discovery: `--list`/by name|rva), `pdb_fetch` (views +
    diff). Mirrors the loop's "select from a list" then "fetch context" steps.

13. **objdiff-core is the precise diff backend (`rich_objdiff`).** Reads the
    delinker `binaries/objdiff/{base,target}/<file>.obj`, runs `diff_objs`, finds
    our symbol, returns `match_percent` + aligned rows. `pdb_fetch --view diff`
    uses it when `--objdiff-{base,target}-dir` are given, else the LCS fallback.
    **Join key = the decorated name from PDB Public symbols**, stored as
    `FunctionEntry.mangled`. The module Procedure `proc.name` is *undecorated*
    (`ns::func`) and does NOT match the COFF symbol (`?func@...`) — found this the
    hard way (objdiff reported "symbol not found" until switched to Public names).
    Verified `contact_test(world*)` = 94.95% (mismatches = register allocation,
    the expected LTO artifact) vs the LCS backend's noisier 90.2%.

## Decisions NOT taken (and why)

1. **Did NOT refactor the delinker into a shared lib.** Working, separately-
   branched tool; lib-ifying it is its own change. We copied a small subset and
   (for objdiff-core) will read its `.obj` files.

2. **Did NOT drop the LCS backend** when adding objdiff-core. It needs no object
   files (works straight off the index), so it stays the fallback when the
   delinker `.obj` dirs aren't supplied or a symbol isn't found.

3. **Did NOT build the agent loop / pragma management / compile orchestration.**
   Explicitly deferred by the owner ("not the agentic loop just yet").

4. **Did NOT implement version history of base attempts.** (Owner: "would be cool
   … but maybe it doesn't need that.") Ties into the failure log and attempt
   tracking, which belong with the loop. Logged in PLAN roadmap.

5. **Did NOT keep the rendered `block` in the index.** Duplicated content and
   blocks the structure/diff views; render on demand instead.

6. **Did NOT add a disassembler beyond iced-x86, nor emit COFF/relocations.**
   Read-only listing; we only name targets, never produce object files.

## Open questions (carried for the owner — see PLAN "Open questions")

Cluster detection source; failure-log schema; diff-distance metric; pragma-
dependency state location; step-2 selection policy; cache key/invalidation.

## Tooling notes / hazards

- Toolchain is a nix flake (nightly Rust); build/run via `nix develop --command
  cargo …`. `cargo` is not on PATH without the dev shell.
- Earlier sessions saw stale/garbled interactive-shell output; confirm
  filesystem facts with exit-code-bearing commands, not `ls` alone.

## Verification (2026-05-30)

- Both indexes build: target 24,467 functions, base 17,434.
- `pdb_fetch` structure/target/base/diff verified on
  `physics/.../ghost_object.cpp`; statement sizes chain; matched `contact_test`
  diffs ~90% (residual = label/symbol text noise), unmatched `create_ghost_object`
  ~96%.
- Clean build of all three bins (only the pre-existing `clap::Parser` warning in
  `lib.rs`).

## Next steps

1. `callees` / `info` views.
2. Index compaction + carcass-comment stripping in base source text.
3. objdiff diff: interleave the source/offset metadata back onto the rows (it is
   currently raw asm only), and surface the structured op stream (not just text).
4. (With the loop, later) version history + machine-readable failure log.
