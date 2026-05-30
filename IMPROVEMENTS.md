# Improvements roadmap — getting more out of the PDB and EXE

This is the menu of extraction/quality improvements identified while building
`pdb_build_info`. Each one is implemented on its own branch off `master` (see
*Branch* below). Nothing here is merged automatically — each branch is meant to
be reviewed and merged independently.

What the tooling already does today:

* **delinking** (`vostok-delinker`) — splits the EXE into per-module COFF objs.
* **structure building** (`pdb_parser`) — headers (types) + annotated source
  stubs with statement addresses.
* **flags** (`pdb_build_info`) — per-project compiler command lines / coarse
  flags, target-vs-base diff.
* **source verification** (`pdb_diff`) — per-file source checksums (MD5/SHA)
  from `DEBUG_S_FILECHECKSUMS`.

Everything below is *not* yet done.

---

## A. Emit the `static` keyword for file-local functions

* **Branch:** `improve/static-keyword`
* **Known debt:** CLAUDE.md "pdb-parser is missing the `static` keyword for
  static functions."
* **Source of truth:** `ProcedureSymbol.global: bool` (S_GPROC32 ⇒ `true`,
  S_LPROC32 ⇒ `false`). A `false` here is an internal-linkage (`static` /
  anonymous-namespace) function.
* **Plan:** carry `global` on `Function`, and in `Function::write` emit
  `static ` for free functions (no class namespace) when `!global`. Skip for
  methods — `static` member functions are a class-level property, not derivable
  from the proc record alone, and writing `static` on an out-of-line member
  definition would be wrong.
* **Files:** `src/gen_sources.rs`.
* **Verify:** regenerate a module known to contain file-local helpers; grep the
  stubs for `static `.

## B. `pe_build_info` — pull metadata straight out of the EXE

* **Branch:** `improve/pe-build-info`
* **Why:** we have both EXEs locally but read nothing from them directly. These
  fields are the definitive toolchain / identity checks.
* **Extracts (target vs base diff, mirroring `pdb_build_info`):**
  * **Rich header** — the masked `DanS…Rich` block between the DOS stub and PE
    header. Decodes to `@comp.id` records: `(build, prodid, count)` — the exact
    cl/link build numbers and how many objects each tool emitted. The single
    best "are we on the identical MSVC build" signal.
  * **PE `TimeDateStamp`** + **debug directory** CodeView entry (PDB GUID/age +
    embedded PDB path) — confirms the PDB↔EXE pairing and build date.
  * **`.rsrc` `VS_VERSIONINFO`** — FileVersion / ProductVersion / CompanyName
    (verifies v0.100b), and the section layout / characteristics.
* **Implementation:** new `src/bin/pe_build_info.rs`. Rich header + version
  resource are parsed by hand (byte offsets); section/data-dir lookup via the
  `object` crate (add dep; already used by `vostok-delinker`).
* **Verify:** run against `survarium.exe` and the base EXE; Rich headers exist
  at offsets 256 / 248 respectively.

## C. Order LOCALS by stack-frame offset

* **Branch:** `improve/locals-frame-order`
* **Why:** CLAUDE.md notes "LOCALS order is arbitrary." The
  `S_BPREL32` / `S_REGREL32` records *do* carry the frame offset; we currently
  read it only to split args from locals (offset sign), then discard it.
* **Plan:** keep the frame offset on each local, and sort the LOCALS block by it
  (ascending = closest-to-frame-pointer first) so the listing reflects the real
  stack layout. Print the offset alongside each local. Args keep source order.
* **Caveat:** under LTCG/optimization the frame is not authoritative — keep the
  existing "approximation" disclaimer.
* **Files:** `src/gen_sources.rs`.

## D. Generate `static_assert` layout guards from the type info

* **Branch:** `improve/layout-asserts`
* **Why:** the PDB knows every struct's exact size and member offsets. Emitting
  `static_assert(sizeof(T)==N)` and `offsetof` checks turns silent layout drift
  in our reconstructed headers into a hard compile error — a cheap, high-value
  matching guard.
* **Source:** TPI `LF_STRUCTURE`/`LF_CLASS` `size`, and `LF_MEMBER` offsets
  (already walked in `gen_headers.rs`).
* **Plan:** emit an optional companion header (e.g. `headers/_layout_asserts.h`,
  behind a `--emit-layout-asserts` flag) with one `static_assert` per known
  size + selected member offsets. Keep it out of the normal stubs so it doesn't
  perturb existing output.
* **Files:** `src/gen_headers.rs`, `src/lib.rs` (flag), `src/type_builder.rs`.

## E. Recover per-function link order from section contributions

* **Branch:** `improve/link-order`
* **Why:** the order functions are laid down in `.text` is the linker's COMDAT
  order; reproducing it matters for the eventual EXE-level (not just per-obj)
  match. The DBI **section contributions** stream maps each RVA range to its
  module, in link order.
* **Plan:** new report (or `pdb_build_info --link-order`) listing, per module,
  its functions in ascending-RVA order with sizes — the link layout. Cross-check
  base vs target ordering.
* **Source:** `DebugInformation::section_contributions()` + the public/proc
  symbol RVA map.

## F. Recover the missing `pstr` / `pvoid` (and friends) typedefs

* **Branch:** `improve/typedefs`
* **Known debt:** CLAUDE.md "pdb-parser is missing typedefs for `pstr`,
  `pvoid`." These are engine aliases (`char*`, `void*`) that the type formatter
  currently expands to the underlying pointer instead of the alias name.
* **Plan:** collect the engine's top-level `S_UDT` / `LF_*` alias entries
  (`pstr`, `pcstr`, `pvoid`, `pbyte`, …) and emit them as real `typedef`s in a
  shared header, and/or teach the formatter to prefer the alias name. Lower
  confidence — touches type formatting, so guard carefully and verify the
  common aliases resolve.
* **Files:** `src/type_builder.rs`, `src/gen_headers.rs`.

## G. Extract RTTI / vftable layout from the EXE

* **Branch:** `improve/rtti-vftables`
* **Why:** MSVC RTTI (`.?AV…@@` type descriptors, `RTTICompleteObjectLocator`,
  class hierarchy descriptors) and the vftables in `.rdata` give class names and
  virtual-function ordering *independently* of the PDB — a cross-check on the
  reconstructed type info and a recovery path for vtables.
* **Plan:** in `pe_build_info` (or a sibling bin), scan `.data`/`.rdata` for
  type-descriptor strings and COL structures, reconstruct the hierarchy, and
  diff against the PDB's `LF_VTSHAPE` / class info. Largest item; depends on B.
* **Files:** `src/bin/pe_build_info.rs` (or new `pe_rtti.rs`).

---

## Branch summary

| Branch | Scope | Confidence |
|---|---|---|
| `improve/static-keyword`     | A | high |
| `improve/pe-build-info`      | B | high |
| `improve/locals-frame-order` | C | medium |
| `improve/layout-asserts`     | D | medium |
| `improve/link-order`         | E | medium |
| `improve/typedefs`           | F | medium-low |
| `improve/rtti-vftables`      | G | low (large) |
