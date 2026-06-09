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
    let n = f.statements.len();
    let _ = writeln!(out, "{}: ; {} statements, 0x{:x} bytes", f.name, n - 2, f.size);
    render_locals_into(&mut out, f);

    // Body rows (strictly between the synthetic frame braces): absolute VA, function
    // offset, signed byte-size to the next statement (the closing `}` for the last
    // row; negative when the next statement sits at a lower address) and source line
    // (+ source text on base). VA is derived from the entry's own image base.
    struct Row {
        va: u32,
        off: u32,
        delta: i64,
        line: u32,
        src: Option<String>,
    }
    let rows: Vec<Row> = (1..n - 1)
        .map(|i| {
            let s = &f.statements[i];
            Row {
                va: f.image_base.wrapping_add(f.rva).wrapping_add(s.off),
                off: s.off,
                delta: f.statements[i + 1].off as i64 - s.off as i64,
                line: s.line,
                src: s.source.clone(),
            }
        })
        .collect();

    // Hex-digit width per column: the widest value present, but never narrower than
    // what the header label needs (label minus its `0x`/`+0x` prefix). Zero-padding
    // to this width keeps the `0x006`/`+0x00b` look yet grows cleanly when the VA,
    // offset or size get larger - nothing is ever a hardcoded width.
    let hexw = |vals: &mut dyn Iterator<Item = u64>, min: usize| {
        vals.map(|v| format!("{:x}", v).len()).max().unwrap_or(1).max(min)
    };
    let da = hexw(&mut rows.iter().map(|r| r.va as u64), "address".len() - 2);
    let dofs = hexw(&mut rows.iter().map(|r| r.off as u64), "offst".len() - 2);
    let dd = hexw(&mut rows.iter().map(|r| r.delta.unsigned_abs()), "size".len() - 3);
    // Column widths follow from the prefixes: `0x`+da, `0x`+dofs, sign+`0x`+dd.
    let (wa, wo, ws) = (2 + da, 2 + dofs, 3 + dd);
    let wl = rows.iter().map(|r| format!("{}", r.line).len()).max().unwrap_or(0).max("line".len());

    let _ = writeln!(out, "{:<wa$}|{:<wo$}|{:^ws$}|{:<wl$}", "address", "offst", "size", "line");
    let _ = writeln!(out, "{}+{}+{}+{}", "-".repeat(wa), "-".repeat(wo), "-".repeat(ws), "-".repeat(wl));
    for r in &rows {
        let sign = if r.delta < 0 { "-" } else { "+" };
        let _ = write!(
            out,
            "0x{:0da$x}|0x{:0dofs$x}|{sign}0x{:0dd$x}|",
            r.va,
            r.off,
            r.delta.unsigned_abs(),
        );
        // Pad `line` only when source text trails it (base), so target rows carry no
        // trailing whitespace.
        match &r.src {
            Some(s) => {
                let _ = writeln!(out, "{:<wl$}\t{s}", r.line);
            }
            None => {
                let _ = writeln!(out, "{}", r.line);
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
