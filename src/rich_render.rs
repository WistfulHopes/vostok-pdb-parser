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

    let stmt_at: HashMap<u32, &crate::rich_context::Statement> =
        f.statements.iter().map(|s| (s.off, s)).collect();

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
    let _ = writeln!(
        out,
        "{}: ; {} statements, 0x{:x} bytes",
        f.name,
        f.statements.len(),
        f.size
    );
    for s in &f.statements {
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
