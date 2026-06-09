# PLAN — rich context & fetch for AI binary matching

## What this is

Tooling that turns the original game's PDB+EXE (and our compiled build's PDB+EXE)
into a **structured, queryable context** an AI uses to binary-match the Vostok
engine. For every function it pairs the disassembly with the source-level
statements that produced it, stores that as structured data, and serves it on
demand as different *views* (target listing, base listing, structure-only, or a
base↔target diff).

It is the data layer under the matching loop described below. The loop itself
(the agent that writes C++, compiles, and iterates) is **out of scope for now** —
we are building the context and fetch primitives it will stand on.

---

## The bigger picture: how AI binary matching should work

(From the project owner's spec, organized. Items marked → are what this tooling
feeds.)

### Standing context the model needs (via SKILL.md)
1. How formatting is done in the project.
2. Which idioms `vostok` developers use — prefer these when matching.
3. Common "source → assembly" mappings, to generate code efficiently.
4. LTO/LTCG reality: argument elision and register-vs-stack calling-convention
   differences are expected and not chased.

### Per-function matching context → (this tooling)
1. Structure of the **target** source from the PDB: number of statements and
   their byte lengths. → `--view structure`, `--view target`.
2. IDA decomp output for target (may be nonsensical under LTO). → not yet.
3. Structure of the **base** (AI-generated) source. → `--view base`,
   `--view structure` on the base index.
4. IDA decomp output for base. → not yet.
5. Base and target **assembly listings**. → `--view target` / `--view base`.
6. Enriched listing = assembly interleaved with the structure above. → the
   listing view already does this; the diff view aligns base vs target.

   The owner's steer: provide **structured output, like objdiff, without
   rendering** — so the model consumes alignment data, not a picture.

### The loop (deferred — not built here)
2. Pick a function from a continuously-updated list.
3. Fetch its source (if any) + all matching context. → `pdb_fetch`.
4. Generate a new version of the source.
5. Compile it.
6. Fetch matching context, analyze.
7. 100% (or LTO-only artifacts) → mark complete, go to 2.
8. Retry budget exhausted → record a machine-readable failure note, go to 2.
9. Otherwise → go to 4.

### Hard realities the loop must respect (deferred, but shape the data model)
- **Linking is ~1 min per change.** Mitigate with batched matching → the diff
  must support **multiple diffs against target in one pass**.
- **Inlining is non-local.** A function inlines in one caller but not another;
  `noinline` pragmas are global side effects. Added-to-fix-A can break-already-
  matched-B → track per-pragma dependents and re-verify on change; the final
  pragma-strip pass must re-test, not just re-compile.
- **Match order:** callees and forced-inline helpers before their callers.
- **Matching unit ≠ always a function:** LTCG inlining makes the output unit
  sometimes a *cluster* of source functions against one target asm span. The
  function list needs a cluster entry.
- **Failure log is machine-readable:** attempt → {source-diff summary, asm diff
  distance, classification, hypothesis}; fed back on retry.
- **Retry budget is diff-distance based:** stop when the distance stops
  shrinking, not at a fixed count.
- **Failure taxonomy:** exact | match-modulo-regalloc/LTO | semantically-equal
  different-codegen | wrong-semantics | structurally-wrong.
- **Pre-filter before model calls:** instruction-count / basic-block / stack-
  frame / rodata-constant deltas reject obviously-wrong source cheaply.
- **Per-file hashes** can drop whole modules from matching.

---

## Architecture

```
 PDB + EXE  ──pdb_rich_context──►  <out>/sources/**      (human-browsable tree)
 (per side)        (build)          <out>/index.jsonl     (structured, queryable)

 index.jsonl ──pdb_rich_query──►   discover: --list / search by name|rva
 (per side)  ──pdb_fetch──────►    fetch views: target | base | structure | diff
```

Two indexes are built — one for **target** (`survarium.{exe,pdb}`, the original
game) and one for **base** (`survarium-dx11-win32-gold.{exe,pdb}`, our build).
Base and target functions **join by signature** (`name`), which is identical
across the two PDBs (RVAs differ, names don't).

"Rebuild completely, then query on top": a full rebuild is ~1.4 s, a query
~0.13 s, so there is no incremental/caching machinery — the build is the refresh
step.

### Data model (`rich_context::FunctionEntry`, one JSON line per function)
```
FunctionEntry {
  name:  String,            // full demangled signature (the join key)
  rva:   u32,               // image-relative; merge key with the line program
  size:  u32,               // function length in bytes
  file:  String,            // source path, '/'-separated (maps to the .obj path)
  statements:   [ Statement { off, size, line, source? } ],
  instructions: [ Instruction { off, len, text, label? } ],
}
```
- `instructions[].text` is the **normalized** mnemonic+operands (branch targets →
  local labels `.1`, call/data targets → recovered symbol names). This is what
  the diff aligns on, **before** any offset/size/source metadata is attached.
- `statements` partition the function: each owns `[off, off+size)`, derived from
  the PDB line program. `source` is the real source line in base mode, `None` in
  target mode (or for inlined/headerless code).

### Components (all in `vostok-pdb-parser/src`)
- `rich_context.rs` — build: PDB+EXE → `FunctionEntry`; writes tree + index.
- `rich_render.rs` — `render_listing` (offset-prefixed asm, `; <0xSIZE> ; <src>`
  on each statement's first instruction) and `render_structure` (statement
  skeleton only).
- `rich_diff.rs` — built-in LCS diff over instruction text → Equal/Delete/Insert
  + match ratio; `render_unified`.
- `rich_objdiff.rs` — operand-aware diff via `objdiff-core` over the delinker
  `.obj`s; returns `match_percent` + structured rows, then interleaves base
  source/offsets onto them.
- `rich_callees.rs` — extract a function's `call` targets and resolve them to
  index signatures (one streaming pass).
- `rich_query.rs` — `search(index, {name substr, rva})`.
- `bin/pdb_rich_context.rs` — build CLI (`--mode base|target`, `--out`).
- `bin/pdb_rich_query.rs` — discovery: `--list` / fetch one by name|rva.
- `bin/pdb_fetch.rs` — `--target-index`/`--base-index`, select by `--function`/
  `--rva`, `--view target,base,structure,diff`; `--objdiff-{base,target}-dir`
  switch the diff to the objdiff-core backend.

---

## Diff

The primitive is an objdiff-style op stream over the two instruction sequences,
computed on normalized text **before metadata**, plus a match ratio (the retry-
budget signal). Two backends:

1. **Built-in LCS** (`rich_diff`) — done. No object files needed; a byte-
   identical function aligns to all-`Equal`. **Known false positives:** synthetic
   label renumbering, and a callee resolving to a different recovered name across
   the two PDBs both show as diffs though the code is equal.

2. **objdiff-core** (`rich_objdiff`) — **done**, operand/relocation-aware, kills
   those false positives. Path: read `binaries/objdiff/{base,target}/<file>.obj`
   (our `FunctionEntry.file` maps straight to them), `diff::diff_objs`, find our
   symbol by its decorated name (`FunctionEntry.mangled`, taken from the PDB
   **Public** symbol — the module symbol is undecorated and does not match the
   COFF name), emit a **fuzzy** `match_percent` + the aligned instruction rows.
   objdiff-core 2.5.0's own symbol `match_percent` is *strict* (any differing
   instruction is a full miss), which understates LTCG code and disagrees with the
   scoreboard's `report.json`; so `rich_objdiff` recomputes a target-byte-weighted
   fuzzy match (`fuzzy_credit`: opcode + per-operand credit, base-only rows weigh 0)
   that tracks `report.json` closely (e.g. `notify_objects_inside` 89.6 vs 89.3).
   LCS stays the no-objfile fallback.

**Rendering:** structured op stream for the model; git-style unified view for
humans. Batched matching will need many diffs against target in one pass.

---

## Views, recap

| view | from | shows |
|---|---|---|
| `target` | target index | offset-prefixed listing, no source |
| `base` | base index | same listing + real source lines inline |
| `structure` | either | statement skeleton: offset, `<size>`, line/source, no asm |
| `diff` | both | aligned base↔target instruction diff + match ratio; the objdiff backend interleaves base source/offsets onto the rows |
| `callees` | function's side | the function's `call` targets, each resolved to its index signature(s) |
| `info` | function's side | PDB-recorded locals (`type name`) |

Planned views: call-site metadata for *indirect* calls (the carcass `CALL SITE
INFO`); optionally `callees` with full bodies, not just signatures.

---

## Deferred / roadmap
- **Version history**: keep the last ~5 base index snapshots so the agent can
  fetch prior attempts and avoid repeating dead ends. (Owner: "would be cool …
  but maybe it doesn't need that.") Cheap to add once attempts are tracked; ties
  into the failure log.
- IDA decomp enrichment (target + base), expected-nonsensical under LTO.
- Batch matching: multiple diffs against target per pass.
- Pre-filters: instruction/BB/stack-frame/rodata deltas before model calls.
- Compact the index (engine-preset filter, short field names, name→offset seek).
- Strip inline carcass `// <addr>|...` comments from base statement source text.
- Demangle data-symbol names (`?g_ph_allocator@...`).

## Out of scope (this tooling)
- The matching agent/loop, pragma management, compile/link orchestration.
- Rewriting the delinker into a shared lib (we read its `.obj` outputs as files).
- RTTI/vftable recovery, layout asserts (separate `IMPROVEMENTS.md` items).

## Verification
- Build base+target indexes; `pdb_fetch` structure/target/base/diff on
  `physics/.../ghost_object.cpp`. Statement sizes chain; a matched function
  diffs near-100% (residual = label/symbol text noise → objdiff-core).
- Determinism: index sorted by (file, rva); byte-stable across runs.

---

## Decisions (2026-05-31, with the project owner)

Five of the six original open questions are settled below; **pragmas stay open**
(item 4). Settled against how the loop actually works (`vostok/docs/binary_matching/`):

1. **Cluster detection — none (diff-reactive).** MSVC 8.0 PDBs carry no
   inline-site records: empirically **0 `S_INLINESITE`** across both PDBs
   (`survarium.pdb` 2396 modules / 47,792 procs, and our base PDB), checked at the
   raw CodeView-kind level with 0 parse errors. Inlining is inferred from the diff
   (missing/extra `call` + large statement-offset delta), per `agentic_loop.md` §5.
   No cluster machinery. (Recorded in the loop's `unanswered_questions.md`.)
2. **Failure log — the loop's existing artifacts, no separate schema.**
   `STATE[..%|PARTIAL]` markers are the machine-greppable layer; the per-function
   `docs/binary_matching/<module>/<fn>.md` is the human detail. (Optional: a fixed
   classification vocabulary in the Outcome line.)
3. **Retry metric — objdiff `match_percent` (primary) + non-equal row count
   (tiebreaker).** Operand/relocation-aware; the row count is only a tiebreaker
   because legitimate LTCG/linker effects (regalloc, stack-slot layout) surface as
   `~` rows. Stop when match % plateaus.
4. **Pragmas — UNRESOLVED (kept open).** The loop doesn't steer inlining today
   (`agentic_loop.md` §5), so there's no dependency state to track yet; revisit if
   inline-control is ever used. The near-term lever it points at — letting the agent
   fetch the asm of a function it suspects is inlined — is logged in the loop's
   `unanswered_questions.md`.
5. **Selection policy — smallest-first, then topological (leaf-first).** Soft
   ordering; each function is a human-reviewed PR. Prefer target-asm size.
6. **Cache key — deferred.** The loop does a full ~1-min rebuild; no source→asm
   cache exists to key. If per-TU incremental lands, key on the preprocessed TU
   (source + headers) + flags + toolchain.

**Tool reconciliation:** `report.json` stays the *scoreboard* (per-function match %
+ regressions per rebuild); `pdb_fetch` is the *microscope* the agent reads for
target asm (loop §2) and the instruction diff (loop §2a). The loop rebuilds the
base index each compile (`generate_rich.py` via `rebuild.py`); the target index is
built once (`setup-toolchain.py`).
