//! Render a structured [`FunctionEntry`] into a text view. Shared by the build
//! (tree files), the query tool and the fetch tool so every surface renders
//! identically. The build stores structure, never strings; rendering is here.

use std::collections::HashMap;
use std::fmt::Write as _;

use crate::rich_context::FunctionEntry;

/// Full listing: offset-prefixed instructions, with `; <0xSIZE> ; <source>`
/// appended on each statement's first instruction. `source` is omitted for
/// target functions (they carry none); the leading offset is the anchor either
/// way. Local labels print on their own line above the instruction.
///
/// (Display only — the exact column layout is intentionally loose; the diff and
/// structure views read the underlying [`FunctionEntry`], not this text.)
pub fn render_listing(f: &FunctionEntry) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}:", f.name);

    // Body-less functions carry only the synthetic frame-brace statements; don't
    // annotate the disassembly with them (keeps base/target listings identical).
    let stmt_at: HashMap<u32, &crate::rich_context::Statement> = if f.is_body_less() {
        HashMap::new()
    } else {
        f.statements.iter().map(|s| (s.off, s)).collect()
    };

    for insn in &f.instructions {
        if let Some(label) = &insn.label {
            let _ = writeln!(out, "{label}:");
        }
        let _ = write!(out, "0x{:02x}:    {}", insn.off, insn.text);
        if let Some(stmt) = stmt_at.get(&insn.off) {
            match &stmt.source {
                Some(src) => {
                    let _ = write!(out, "\t; <0x{:x}> ; {src}", stmt.size);
                }
                None => {
                    let _ = write!(out, "\t; <0x{:x}>", stmt.size);
                }
            }
        }
        let _ = writeln!(out);
    }
    out
}

/// Function info: the PDB-recorded locals (`type  name`). Approximate under LTO
/// — some are optimized out and register locals may overlap arguments.
pub fn render_info(f: &FunctionEntry) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}:", f.name);
    let _ = writeln!(out, "; locals ({}) — PDB-recorded, approximate under LTO", f.locals.len());
    for l in &f.locals {
        let _ = writeln!(out, "  {}\t{}", l.ty, l.name);
    }
    out
}

/// Structure-only: the statement skeleton (offset, size, line, source) without
/// any disassembly — "the amount of statements and their length", which is the
/// cheap structural signal a matcher can compare before generating code.
pub fn render_structure(f: &FunctionEntry) -> String {
    let mut out = String::new();

    // Body-less functions (empty `{}`) carry only the synthetic frame braces,
    // which the two PDBs encode with a varying statement count (1 or 2). Normalize
    // every form to an empty body: header line only, zero statement rows.
    if f.is_body_less() {
        let _ = writeln!(out, "{}: ; 0 statements, 0x{:x} bytes", f.name, f.size);
        render_locals_into(&mut out, f);
        return out;
    }

    // The FIRST and LAST statements are the synthetic frame braces (`{` and `}`);
    // the real body is strictly between them (the same skip-first-last rule that
    // `gen_sources` and the structure-diff use). Show only those body statements,
    // and the locals, so this view matches the carcass's `// FUNCTION BODY` +
    // `// LOCALS` (each declared local is its own statement under /Od).
    let body = &f.statements[1..f.statements.len() - 1];
    let _ = writeln!(
        out,
        "{}: ; {} statements, 0x{:x} bytes",
        f.name,
        body.len(),
        f.size
    );
    render_locals_into(&mut out, f);
    for s in body {
        match &s.source {
            Some(src) => {
                let _ = writeln!(out, "0x{:02x}  <0x{:x}>  {src}", s.off, s.size);
            }
            None => {
                let _ = writeln!(out, "0x{:02x}  <0x{:x}>  L{}", s.off, s.size, s.line);
            }
        }
    }
    out
}

/// Append the PDB-recorded locals as `; locals (N):` then `;   <type>\t<name>`
/// rows (nothing when there are none). Approximate under LTO - some are optimized
/// out and register locals may overlap arguments.
fn render_locals_into(out: &mut String, f: &FunctionEntry) {
    if f.locals.is_empty() {
        return;
    }
    let _ = writeln!(out, "; locals ({}):", f.locals.len());
    for l in &f.locals {
        let _ = writeln!(out, ";   {}\t{}", l.ty, l.name);
    }
}
