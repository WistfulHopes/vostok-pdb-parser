# WORK LOG — `pdb_rich_context`

Running log of decisions **taken** and **not taken**, with rationale. Newest at
the bottom of each section.

## Context established

* Worktree `rich-context-binary` created off the repo (branch
  `worktree-rich-context-binary`). Task: a new binary producing rich,
  disassembly-interleaved-with-source context for binary matching.
* Located inputs:
  * target: `/nix/store/…-survarium-game/survarium.{exe,pdb}` (also copied in
    `vcproj2ninja/`).
  * base: `vostok/binaries/Win32/survarium-dx11-win32-gold.{exe,pdb}`.
    (Note: the `.exe` there was 0 bytes at inspection time — a rebuild is needed
    before base smoke-testing; the `.pdb` is present at 72 MB.)
* Confirmed the two reusable halves:
  * `vostok-pdb-parser/src/gen_sources.rs` already turns the PDB line program
    into per-function `Statement{ rva, line_start, depth }` — the source half.
  * `vostok-delinker/src/object_files.rs` already slices per-function `.text`
    bytes and walks them with `iced-x86` — the disassembly half.
  * Both key functions by the **same RVA** (`text.rva + proc.offset`), so
    instruction→statement mapping is an exact sorted merge, not a heuristic.

## Decisions TAKEN

1. **Host the new bin in `vostok-pdb-parser`, not `vostok-delinker`.**
   Why: the *source* half (signatures via `PdbParser`/`formatter`, statement +
   line extraction in `gen_sources`, the `Files`/`utils_fs` structure-tree
   writer) all live here and are the richer, harder-to-move pieces. The delinker
   half we actually need is a small, self-contained subset (decode loop + symbol
   RVA maps + section info). Cheaper to bring that subset here than to move
   pdb-parser's type machinery there.

2. **Copy the minimal delinker subset rather than depend on the delinker crate.**
   The delinker exposes only a `main` (binary crate, no `lib.rs`); its modules
   are private. Rather than refactor it into a library now (risky, touches a
   working tool), port just: PE/PDB open + `Env`/`SecInfo` (section info), the
   `PdbSymbols` RVA maps, and the iced-x86 decode pattern. Recorded as debt in
   "NOT taken #1".

3. **Reuse the existing structure-tree output layout** (`utils_fs::open_file` +
   `Files`), one file per source file, functions in RVA order. Keeps the new
   artifact diffable side-by-side with `vostok-structure` and the carcass.

4. **Offsets are function-relative hex** (`0x00`, `0x0C`, …), matching the
   example and the existing `FUNCTION BODY` carcass convention, instead of
   absolute VAs. Absolute VAs remain available but noisy; function-relative is
   what makes base/target blocks line up.

5. **Statement byte-size = span to the next statement's RVA** (last statement to
   function end). This is what the `; 0xNN` annotation in the brief's example
   encodes, and it's directly derivable from the already-sorted statement RVAs.

6. **Synthetic local labels (`.1`, `.2`)** for in-function branch targets, with
   branch operands rewritten to them. Makes control flow readable without
   absolute addresses and keeps base/target output stable under relocation.

7. **Single source of truth for the function list = module Procedure/Thunk
   symbols** (delinker's approach), not the global public symbol table. Public
   symbols lack reliable source-file attribution; module symbols give us the
   line program and file in the same pass.

8. **Target mode prints `'<line>'` placeholders, identical structure otherwise.**
   The brief states target == base minus source text; keeping byte/offset/label
   logic mode-independent means the two outputs are directly comparable.

9. **Per-instruction annotation is the instruction's SIZE, not its offset.**
   (User correction.) The brief's `0xNN:` leading column was being emitted as
   offset-from-function-start; that drifts on any size change and isn't what
   matching compares. Replaced with each instruction's byte length. These sum
   exactly to the statement's total — verified on `bt_ghost_object::remove`
   (3+4+2+3+3+4+2 = 0x15) and `contact_test` blocks. The merge logic (statement
   ownership of `[start,end)`) is unchanged and the sums prove it correct.

10. **Sizes are rendered as `; 0xNN bytes` — hex value plus the literal word
    `bytes`.** (User goal: "as straightforward as possible".) Hex keeps it
    consistent with IDA/objdiff and the disasm operands; the word `bytes`
    disambiguates it from an address/offset. Applies to both the statement-total
    header and each instruction line.

11. **Target statement header = size only (no line number).** (User: the line
    number is "noise … useless for matching".) Base shows the real source line
    (read from `source_root` via the PDB line number); target, having no source,
    shows just `; N bytes`. Statement-header *format* otherwise left open — user
    said "format let's discuss later".

12. **No-source statement header is anchored by `[0xNN]` = function-relative
    offset.** (User: "instead of source code we can specify address [0xFF]";
    then "we can also specify it for target".) Applies to every statement whose
    source text is unavailable — all target statements, and base statements from
    inlined/headerless code. Replaces the bare `; 0xNN bytes` line. Chosen
    function-relative offset (small hex, matches the user's `[0xFF]` example,
    comparable base↔target) over absolute VA (would differ between binaries; an
    easy switch if IDA-jump utility is wanted later). Offsets chain to the sizes
    (`0x0+0x3=0x3`, `0x3+0x9=0xc`, …), confirming the merge once more.

Smoke tests (both pass, 2026-05-30):
* target: `vcproj2ninja/survarium.{exe,pdb}`, `--engine-path c:\survarium\sources`
* base:   `vostok/binaries/Win32/survarium-dx11-win32-gold.{exe,pdb}`,
  `--engine-path z:\home\sheep\projects\surv-decomp\vostok\sources`,
  `--source-root vostok/sources` — real source lines render; disasm+sizes are
  byte-identical to target for the matched `ghost_object.cpp`.

## Decisions NOT taken (and why)

1. **Did NOT refactor `vostok-delinker` into a shared library.** Tempting (would
   avoid copy-paste), but it's a working, separately-branched tool and lib-ifying
   it is its own reviewable change. Deferred; the copied subset is small and
   isolated. Revisit if the duplication grows.

2. **Did NOT emit COFF / reuse the relocation machinery.** We only *read* call
   and data targets to *name* them in comments; we never produce object files.
   The relocation-emission code (`relocs.rs`, `add_relocation_*`) is irrelevant
   to a read-only listing and would add large surface area.

3. **Did NOT fold this into the existing `pdb_parser`/`gen_sources` carcass
   output.** That writer is tuned for the compile-able stub carcass (LOCALS,
   CONSTANTS, FUNCTION BODY comment block). Mixing real disassembly into it would
   muddy both. New bin, shared helpers, separate output.

4. **Did NOT add a disassembler beyond `iced-x86`.** It's already a delinker dep,
   handles 32-bit x86, and gives flow-control + branch-target info we need for
   labels. No reason to introduce capstone/zydis.

5. **Did NOT attempt source-text recovery for target.** No source exists for the
   original game; inventing/decompiling it is out of scope and would defeat the
   purpose (the AI should reason from real disasm + line numbers, not a guess).

6. **Did NOT special-case LTCG-optimized modules.** Per CLAUDE.md these are
   intentionally not matched yet; the listing still renders them correctly
   (offsets/sizes are byte-accurate regardless), we just don't add LTO-specific
   annotations.

## Tooling notes / hazards

* The interactive shell returned **stale/garbled output** several times this
  session (duplicated reads, phantom file listings). Verified ground truth with
  `git ls-files`, `git status --porcelain`, and `stat` before trusting any `ls`.
  Phantom `src/disasm.rs` etc. listings were confirmed non-existent via `stat`.
  Lesson: confirm filesystem facts with non-cacheable, exit-code-bearing commands.
* `pkill -9 find` was denied by the sandbox (would affect other PIDs) — avoid
  process-wide signals; scope cleanup to known background-task IDs instead.

## Next steps (implementation order)

1. `src/statements.rs` — lift the statement-extraction loop from
   `gen_sources::Module::build` into a reusable `for_symbol(program, offset)`.
2. `src/disasm.rs` — `decode(bytes, va) -> Vec<DecodedInsn>` + label assignment,
   patterned on `resolve_relative_relocations`.
3. Port `Env`/`SecInfo` + `PdbSymbols` subset.
4. `src/rich_context.rs` — orchestrate + merge + write via `Files`/`utils_fs`.
5. `src/bin/pdb_rich_context.rs` — CLI.
6. Smoke-test (base, then target); cross-check offsets vs the carcass.
