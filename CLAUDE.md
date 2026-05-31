# vostok-pdb-parser — rich context & fetch for AI binary matching

This file tells an AI agent **how to use the rich-context query system** to
binary-match the Vostok engine. For *why* it is built the way it is, read
`PLAN.md` (design) and `WORK.md` (decision log). This file is the operator's
manual: commands in, views out, and how to read them.

---

## What this tool gives you

For any function in the original game (the **target**) or in our compiled build
(the **base**), it pairs the **disassembly** with the **source-level statements**
that produced it, and serves that on demand as different *views*:

| view | needs | shows |
|---|---|---|
| `target` | target index | original-game listing, offset-prefixed, no source |
| `base` | base index | our build's listing **with the real C++ source line inline** |
| `structure` | either | statement skeleton: offset, byte size, source/line — no asm |
| `diff` | both | base↔target aligned instruction diff + a match % |
| `callees` | one side | the function's `call` targets, resolved to full signatures |
| `info` | one side | PDB-recorded locals (`type name`) — approximate under LTO |

The whole point: feed the model **structured, comparable context** (sizes,
alignment, source mapping) instead of a screenshot of objdiff.

---

## Prerequisites

The toolchain is a nix flake (nightly Rust). `cargo` is **not** on `PATH`
without the dev shell, so every command is prefixed:

```bash
nix develop --command cargo run --release --quiet --bin <binary> -- <args>
```

Run from the `vostok-pdb-parser` repo root. Sibling repos are reached with `..`
(`../vostok`, `../vcproj2ninja`). The three binaries:

- `pdb_rich_context` — **build** an index from a PDB+EXE.
- `pdb_rich_query` — **discover** functions in an index (`--list` / by name|rva).
- `pdb_fetch` — **fetch** views for one function (joins base↔target).

---

## The model: "rebuild completely, then query on top"

There is no incremental cache. A full index rebuild is ~1.4 s; a query is
~0.13 s. So: build the index once (the refresh step), then run as many queries
against `index.jsonl` as you like — queries never re-parse the PDB.

```
PDB + EXE ──pdb_rich_context──► out/<side>/sources/**   (browsable tree)
                                out/<side>/index.jsonl   (one JSON line per fn)
index.jsonl ──pdb_rich_query──► discover
            ──pdb_fetch───────► views (+ diff)
```

Two indexes exist — **target** and **base** — and they **join by signature
name**: the RVA differs between the two PDBs, the demangled name does not.

---

## Step 1 — Build the indexes

Build the **target** (original game; no sources, statements carry line-number
placeholders):

```bash
nix develop --command cargo run --release --quiet --bin pdb_rich_context -- \
  --pdb  ../vcproj2ninja/survarium.pdb \
  --exe  ../vcproj2ninja/survarium.exe \
  --engine-path 'c:\survarium\sources' \
  --mode target --out out/target
```

Build the **base** (our compiled build; `--source-root` lets it read the real
C++ line for each statement):

```bash
nix develop --command cargo run --release --quiet --bin pdb_rich_context -- \
  --pdb  ../vostok/binaries/Win32/survarium-dx11-win32-gold.pdb \
  --exe  ../vostok/binaries/Win32/survarium-dx11-win32-gold.exe \
  --engine-path 'z:\home\sheep\projects\surv-decomp\vostok\sources\' \
  --source-root ../vostok/sources \
  --mode base --out out/base
```

Rebuild the **base** index after every recompile (its asm changed); the target
index only changes if you swap the reference binary. Current sizes: target
24,467 functions, base 17,434.

> `--out` writes both `index.jsonl` and a human-browsable `sources/` tree. Omit
> `--out` to dump one side to stdout for a quick look.

---

## Step 2 — Discover a function

`pdb_rich_query --list` prints `rva  file  signature` for every match of a
case-insensitive substring. Use it to find the exact RVA when a name is
overloaded:

```bash
nix develop --command cargo run --release --quiet --bin pdb_rich_query -- \
  --index out/target/index.jsonl --function contact_test --list
```
```
0x6eec80  vostok/game_core/sources/collision_geometry.cpp	bool survarium::collision_geometry::contact_test()
0x573750  vostok/physics/sources/ghost_object.cpp	bool vostok::physics::bt_ghost_object::contact_test(vostok::physics::world*)
0x573cc0  vostok/physics/sources/ghost_object.cpp	void vostok::physics::bt_ghost_object::contact_test(vostok::physics::world*, ...)
...
```

`--function` is a loose substring (good for discovery); `--rva 0x573750` is the
exact pick. Drop `--list` to print the function body (the `target`/`base`
listing) directly.

---

## Step 3 — Fetch views

`pdb_fetch` takes `--target-index` and/or `--base-index`, a selector
(`--function` substring or `--rva` exact), and `--view` (comma-separated).
Default view: `diff` if both indexes are given, else the side you supplied.

When both indexes are given, the **target match is resolved first, then base is
joined by that exact name** — so a loose `--function` still pairs the same
function on both sides.

### `target` — what to match

```bash
pdb_fetch --target-index out/target/index.jsonl --rva 0x573750 --view target
```
```
bool vostok::physics::bt_ghost_object::contact_test(vostok::physics::world*):
0x00:    sub   esp, 1Ch	; <0x3>
0x03:    mov   ecx, [eax+10h]	; <0x9>
0x06:    mov   ecx, [ecx+130h]
0x0c:    mov   edx, [ecx]	; <0xd>
0x0e:    mov   eax, [edx+18h]
0x11:    push  ebx
...
```

### `base` — our build, with source inline

```bash
pdb_fetch --target-index out/target/index.jsonl \
          --base-index   out/base/index.jsonl \
          --rva 0x573750 --view base
```
```
bool vostok::physics::bt_ghost_object::contact_test(vostok::physics::world*):
0x00:    sub   esp, 1Ch	; <0x3> ; {
0x03:    mov   ecx, [eax+10h]	; <0x16> ; btBroadphasePairArray& bt_pair_array = m_bt_object->getOverlappingPairCache( )->getOverlappingPairArray( );
0x06:    mov   ecx, [ecx+130h]
...
0x19:    mov   eax, [ebp+4]	; <0x3> ; s32	pairs_count = bt_pair_array.size( );
0x1c:    xor   ebx, ebx	; <0x12> ; for ( s32 i = 0 ; i < pairs_count ; ++i )
```

### `structure` — the cheap structural signal

Statement count, each statement's byte size, and its source (base) or line
placeholder (target). Compare this **before** generating code: same number of
statements with the same byte sizes is a strong "you're on the right track".

```bash
pdb_fetch --target-index out/target/index.jsonl --rva 0x573750 --view structure
```
```
bool vostok::physics::bt_ghost_object::contact_test(...): ; 17 statements, 0x102 bytes
0x00  <0x3>   L77
0x03  <0x9>   L79
0x0c  <0xd>   L80
0x19  <0x3>   L82
0x1c  <0x12>  L83
...
```

### `diff` — aligned base↔target + match %

Two backends. The **LCS** backend needs no object files (works straight off the
indexes) but counts cosmetic text differences (label/symbol renames) as
mismatches:

```bash
pdb_fetch --target-index out/target/index.jsonl \
          --base-index   out/base/index.jsonl \
          --rva 0x573750 --view diff
```

The **objdiff-core** backend is operand/relocation-aware (the real match
signal). Point it at the delinker's `.obj` dirs; it interleaves the base source
back onto the rows:

```bash
pdb_fetch --target-index out/target/index.jsonl \
          --base-index   out/base/index.jsonl \
          --rva 0x573750 --view diff \
          --objdiff-base-dir   ../vostok/binaries/objdiff/base \
          --objdiff-target-dir ../vostok/binaries/objdiff/target
```
```
bool vostok::physics::bt_ghost_object::contact_test(vostok::physics::world*):
; objdiff match 94.95%
{	; <0x3>
  0x00: sub esp, 1Ch
btBroadphasePairArray& bt_pair_array = ...->getOverlappingPairArray( );	; <0x16>
  0x03: mov ecx, [eax+10h]
  ...
~ 0x34: mov edx, [ecx+34h]           -> mov eax, [ecx+34h]
~ 0x37: mov ecx, [edx+4Ch]           -> mov ecx, [eax+4Ch]
```

Read the rows: `  ` = equal, `~ base -> target` = same slot different
instruction (here, register allocation — `edx` vs `eax`), `- ` = base-only,
`+ ` = target-only. The `~` rows above are **expected LTO artifacts** (regalloc
differs), not real mismatches — see "LTO reality" below.

If the objdiff backend can't find the symbol it prints a note and falls back to
the LCS text diff automatically.

### `callees` — what a body depends on

Match callees and forced-inline helpers **before** their callers. This view
lists the function's `call` targets, each resolved to the index signature(s):

```bash
pdb_fetch --target-index out/target/index.jsonl --rva 0x573cc0 --view callees
```
```
void vostok::physics::bt_ghost_object::contact_test(..., contact_test_predicate&):
; callees (1)
  btCollisionWorld::contactPairTest	-> void btCollisionWorld::contactPairTest(btCollisionObject*, btCollisionObject*, btCollisionWorld::ContactResultCallback&)
```

`callees (0)` means every call was **indirect** (`call eax`) — a vtable/function
pointer the disassembler can't name. Unresolved targets print `(unresolved)`.

### `info` — PDB-recorded locals

```bash
pdb_fetch --target-index out/target/index.jsonl --rva 0x573750 --view info
```
```
bool vostok::physics::bt_ghost_object::contact_test(vostok::physics::world*):
; locals (3) — PDB-recorded, approximate under LTO
  const s32	pairs_count
  s32	i
  btAlignedObjectArray<btPersistentManifold *>	manifold_results
```

Approximate: under LTO some locals are optimized out and register locals can
overlap arguments.

---

## Notation cheat-sheet

- `0x1c:` — the instruction's **offset from the function start**. Differs
  between base and target builds; it is an anchor, not something to match.
- `; <0xNN>` — the **byte size of the statement** (hex), printed on the
  statement's first instruction. Sizes sum to the function length. Marked
  `<...>` so it doesn't read as an address (operands are also hex).
- `; <0xNN> ; <source>` — base only: the size, then the real C++ line that
  compiled to this statement. Target carries no source; the offset is its anchor.
- `.1:` / `.2:` on their own line — synthetic **local labels** for in-function
  branch targets (branch operands are rewritten to them).
- `[0xNN]` in a diff statement header — a base statement with no source
  (inlined/headerless); the offset stands in for it.
- match % (LCS `; N/M instructions equal (P%)`, or objdiff `; objdiff match P%`)
  — the **retry-budget signal**. Watch it grow across attempts; stop when it
  stops growing, not at a fixed count.

---

## A matching session, end to end

```bash
# 0. (re)build base after a compile; target is already built
nix develop --command cargo run --release --quiet --bin pdb_rich_context -- \
  --pdb ../vostok/binaries/Win32/survarium-dx11-win32-gold.pdb \
  --exe ../vostok/binaries/Win32/survarium-dx11-win32-gold.exe \
  --engine-path 'z:\home\sheep\projects\surv-decomp\vostok\sources\' \
  --source-root ../vostok/sources --mode base --out out/base

# 1. find the function and its exact RVA
nix develop --command cargo run --release --quiet --bin pdb_rich_query -- \
  --index out/target/index.jsonl --function contact_test --list

# 2. study the target: structure first (cheap), then the listing
nix develop --command cargo run --release --quiet --bin pdb_fetch -- \
  --target-index out/target/index.jsonl --rva 0x573750 --view structure,target

# 3. line up dependencies and recorded locals
nix develop --command cargo run --release --quiet --bin pdb_fetch -- \
  --target-index out/target/index.jsonl --rva 0x573750 --view callees,info

# 4. write/adjust the C++, recompile, rebuild base (step 0), then diff:
nix develop --command cargo run --release --quiet --bin pdb_fetch -- \
  --target-index out/target/index.jsonl --base-index out/base/index.jsonl \
  --rva 0x573750 --view diff \
  --objdiff-base-dir ../vostok/binaries/objdiff/base \
  --objdiff-target-dir ../vostok/binaries/objdiff/target
# repeat 4 until match % stops climbing; residual `~` regalloc rows are LTO, fine.
```

---

## LTO reality (don't chase these)

Most target modules were built with `/LTCG`. The objdiff `~` (Replace) rows that
remain on an otherwise-matched function are usually:

- **register allocation** differences (`mov edx,...` vs `mov eax,...`),
- arguments the optimizer elided because they were constant call-site-wide,
- stack-vs-register calling-convention differences.

These are **expected** and not matched right now. Match the *body* (the shape,
the statement sizes, the instruction sequence); a function at ~95% with only
regalloc `~` rows is considered matched.

---

## Gotchas

- **`cargo` needs the dev shell** — always prefix `nix develop --command`.
- **Rebuild base after every compile**, or the diff compares stale asm.
- **Overloaded names** — `--function` returns the first of several matches (it
  prints a note); use `--rva` from `--list` to pin the exact one.
- **`diff` needs both indexes**; the objdiff backend additionally needs the
  delinker `.obj` dirs (`build_base.bat` / `build_target.bat` produce them).
- **Join key is the demangled name.** The objdiff backend looks symbols up by
  the *decorated* (mangled) COFF name stored in the index from the PDB **Public**
  symbol — the module symbol is undecorated and won't match the `.obj`.
- **Indexes are not committed.** Treat `out/` as a build artifact; rebuild it
  (it's fast). Build to a scratch dir if you don't want it in the tree.

---

## Where things live

- `src/rich_context.rs` — build: PDB+EXE → `FunctionEntry`, writes tree + index.
- `src/rich_render.rs` — `render_listing` / `render_structure` / `render_info`.
- `src/rich_diff.rs` — LCS text diff (no-objfile fallback).
- `src/rich_objdiff.rs` — objdiff-core operand-aware diff.
- `src/rich_callees.rs` — extract & resolve `call` targets.
- `src/rich_query.rs` — `search(index, {name, rva})`.
- `src/bin/{pdb_rich_context,pdb_rich_query,pdb_fetch}.rs` — the three CLIs.

The matching agent/loop, pragma management, and compile/link orchestration are
**out of scope** for this tool — it is the data layer they stand on. See
`PLAN.md` for the open questions (cluster detection, failure-log schema,
diff-distance metric) still awaiting owner decisions.
