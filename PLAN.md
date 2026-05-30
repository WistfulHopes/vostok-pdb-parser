# PLAN — `pdb_rich_context`: rich per-function context for binary matching

## Goal

A new binary that, for every engine function, emits a single human/AI-readable
block that **interleaves the disassembly with the source-level statements** that
produced it, annotated with byte sizes and offsets. This is the artifact the AI
(and humans) read while matching: it puts "what the source statement was" next
to "what machine code it compiled to", per statement, in one place.

Two modes, same layout:

* **base** (`survarium-dx11-win32-gold.{exe,pdb}`) — source files exist on disk,
  so each statement line shows the *actual source text*.
* **target** (`survarium.{exe,pdb}`, original game) — no source available, so the
  statement line shows only its line number / a placeholder; everything else
  (disassembly, sizes, offsets) is identical.

### Target output (mirrors the brief's example)

```
int64_t sum_range(int64_t n):
{                ; 0x16
0x00:    push rbp
0x01:    mov  rbp, rsp
0x04:    sub  rsp, 16
0x08:    mov  [rbp-8], rdi

int64_t sum = 0  ; 0x03
0x0C:    xor  rax, rax

while (i <= n) { ; 0x06
.1:
0x16:    cmp  rcx, [rbp-8]
0x1A:    jg   .2
...
}                ; 0x01
0x28:    ret
```

* The text before `;` is the source statement (or `'<line>'` placeholder for
  target). The `; 0xNN` is the **byte size of that statement's instruction run**.
* `0xNN:` per instruction is the **offset from function start** (matches the
  existing structure carcass convention; see `gen_sources.rs` FUNCTION BODY).
* Local jump targets get synthetic labels (`.1`, `.2`) so reading flow needs no
  absolute addresses.

## What already exists (reuse, do not reinvent)

| Capability | Where | Reuse for |
|---|---|---|
| PDB→per-function **statements** (RVA, source line, scope depth) via the module line program | `vostok-pdb-parser/src/gen_sources.rs` (`Module::build`, `Statement`) | statement boundaries + line numbers |
| Function discovery (S_GPROC32/S_LPROC32 + thunks), per-function `.text` bytes, source-file resolution under `engine_path` | `vostok-delinker/src/{object_files.rs,pdb_symbols.rs,main.rs}` (`Env`, `get_function_location`, `add_function_symbol`) | function list, byte slices, file grouping |
| x86 instruction-flow walk (follow branches/calls) | `vostok-delinker/src/object_files.rs` (`resolve_relative_relocations` via `iced-x86`) | disassembly decode loop pattern |
| Symbol/RVA maps (functions, strings, constants, statics) | `vostok-delinker/src/pdb_symbols.rs` (`PdbSymbols`) | resolve call/data targets to names in the listing |
| PE/PDB open + section info | `vostok-delinker/src/main.rs` (`Env::build`) | image base, `.text/.rdata/.data` SecInfo |
| Type/function signature formatting | `vostok-pdb-parser/src/{pdb_parser.rs,formatter.rs}` | the function signature header line |
| `iced-x86` formatting | new dep usage (crate already in delinker) | mnemonic/operand text |

The crucial alignment: **both** `gen_sources.rs` and `object_files.rs` key
functions by the same RVA (`text.rva + proc.offset.offset`). Statements carry
RVAs; instructions carry IPs. So mapping instructions→statements is a sorted
merge on RVA — no fuzzy matching needed.

## Where the code lives

This binary needs both PDB *type/line* parsing (pdb-parser side) **and** PE byte
slicing + iced-x86 (delinker side). The delinker logic is the harder half and is
not currently exposed as a library. Decision (see WORK.md): **build the new bin
inside `vostok-pdb-parser`** and bring over the minimal delinker pieces, because
pdb-parser already has the richer type/line tooling (`PdbParser`, `gen_sources`)
that the listing's *source* half needs, whereas the delinker half we need is a
small, well-isolated subset (decode loop + symbol map + section info).

New files (all under `vostok-pdb-parser/src/`):

* `src/bin/pdb_rich_context.rs` — CLI (`--pdb`, `--exe`, `--out`, `--engine-path`,
  `--mode base|target`). Mirrors `pdb_build_info.rs` / delinker `Cli`.
* `src/rich_context.rs` — orchestration: open PE+PDB, build per-module function
  map, for each function build a `RichFunction` and write it.
* `src/disasm.rs` — thin wrapper over `iced-x86`: decode a `&[u8]` at a base IP
  into `Vec<DecodedInsn { offset, len, ip, text, flow, branch_target }>`, plus
  synthetic local-label assignment.
* `src/statements.rs` — extract `Vec<Statement{ rva, line, depth, end_rva }>`
  for one function from the module line program (lifted/trimmed from the parsing
  half of `gen_sources::Module::build`, without the file-writing concerns).

Library wiring: add `pub mod rich_context; pub mod disasm; pub mod statements;`
to `lib.rs`. No changes to existing bins' behavior.

## Algorithm (per function)

1. **Discover functions** (delinker pattern): iterate DBI modules → module
   symbols → `Procedure`/`Thunk`; resolve source file via
   `get_function_location`; keep only files under `engine_path` (base) or keep
   all engine-attributed ones (target). Record `(name, rva, size)`.
2. **Slice bytes**: `text.data[off .. off+size]`.
3. **Decode**: `disasm::decode(bytes, va_base)` → instruction list with offsets.
4. **Statements**: `statements::for_symbol(program, proc.offset)` → sorted by
   RVA, with each statement's end = next statement's start (last ends at
   func_end). Carry `depth` for the `{`/block markers.
5. **Source text** (base only): read the source file once (cache by path), index
   by line number; map each statement → its source line text. Target: text is
   `'<line>'`.
6. **Merge**: walk instructions in offset order; for each statement bucket
   `[start,end)` print the statement header (`source_text  ; 0x<size>`) then its
   instructions (`0x<off>: <mnemonic> <ops>`). Instructions before the first
   statement / in gaps attach to the nearest preceding statement (these are
   prologue/inlined-call regions — flag large gaps like the existing carcass
   does).
7. **Labels**: pre-scan branch targets that land inside the function; assign
   `.1,.2,…` in address order; emit a label line before the target instruction
   and rewrite branch operands to the label.
8. **Call/data names**: for `call`/branch/data refs whose target RVA is in
   `PdbSymbols.{functions,strings,constants,statics}`, append a `; -> name`
   comment (reuse delinker's closest-symbol selection).

## Output file layout

Mirror `vostok-structure`: one output file per source file (path taken from the
line program, `\`→`/`, nested under `out/`), functions in RVA order within the
file, each as the block above. Reuse `utils_fs::open_file` + the `Files` cache so
the directory tree matches the existing structure exactly. A `--single-file`
option dumps everything to one stream for quick diffing.

## Verification

* **Smoke**: run base mode against `vostok/binaries/Win32/survarium-dx11-win32-gold.*`
  with `--engine-path vostok/sources`; pick a small known-matched function
  (e.g. `physics/.../ghost_object.cpp`) and eyeball that statement sizes sum to
  function size and offsets are contiguous.
* **Cross-check offsets** against the existing carcass `FUNCTION BODY` block for
  the same function — the per-statement RVAs must agree.
* **Target**: run against `survarium.{exe,pdb}`; confirm identical structure with
  `'<line>'` placeholders and that disassembly decodes cleanly to `ret`.
* **Determinism**: byte-identical output across two runs.

## Out of scope (this binary)

* Rewriting the delinker to be a shared lib (tracked separately; we copy the
  minimal subset now — see WORK.md "not taken").
* RTTI/vftable recovery, layout asserts, link-order — separate roadmap items in
  `IMPROVEMENTS.md`.
* Relocation *emission* (COFF) — we only *read* targets to name them, we do not
  produce object files.
