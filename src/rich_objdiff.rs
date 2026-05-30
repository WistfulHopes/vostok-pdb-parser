//! Precise base↔target diff via `objdiff-core`: operand/relocation-aware, so the
//! match percentage is meaningful (a callee resolving to a different name, or a
//! relocation, doesn't count as a mismatch the way the text LCS backend would).
//!
//! Inputs are the delinker's per-unit COFF objects
//! (`binaries/objdiff/{base,target}/<file>.obj`); our [`FunctionEntry::file`]
//! maps straight to them and [`FunctionEntry::mangled`] is the COFF symbol name
//! to look up. objdiff matches symbols across the two objects by name; we pull
//! out the one we asked for and render its aligned instruction diff.

use std::fmt::Write as _;
use std::path::Path;

use objdiff_core::diff::{diff_objs, DiffObjConfig, ObjDiff, ObjInsDiff, ObjInsDiffKind, ObjSymbolDiff};
use objdiff_core::obj::read::read;
use objdiff_core::obj::ObjInfo;

pub struct ObjdiffResult {
    /// Instruction-level match percent (0..100). The retry-budget signal.
    pub match_percent: f32,
    /// Rendered aligned diff listing.
    pub listing: String,
}

fn anyhow_to_err(e: anyhow::Error) -> crate::Error {
    crate::Error::new(format!("{e:#}"))
}

/// Diff the function named `mangled` (a COFF symbol name) between the two object
/// files. Returns `None` if either object lacks the symbol (caller can fall back
/// to the text diff).
pub fn diff(base_obj: &Path, target_obj: &Path, mangled: &str) -> crate::Result<Option<ObjdiffResult>> {
    let cfg = DiffObjConfig::default();
    let base = read(base_obj, &cfg).map_err(anyhow_to_err)?;
    let target = read(target_obj, &cfg).map_err(anyhow_to_err)?;

    let res = diff_objs(&cfg, Some(&base), Some(&target), None).map_err(anyhow_to_err)?;
    let (Some(base_diff), Some(target_diff)) = (res.left.as_ref(), res.right.as_ref()) else {
        return Ok(None);
    };

    let Some(bsym) = find_symbol(base_diff, &base, mangled) else {
        return Ok(None);
    };
    let match_percent = bsym.match_percent.unwrap_or(0.0);

    let mut listing = String::new();
    let _ = writeln!(listing, "; objdiff match {match_percent:.2}%  {mangled}");

    // objdiff aligns the two instruction streams into equal-length rows when the
    // symbols are paired; zip them for a unified view. If unpaired, show base.
    match bsym.target_symbol.map(|r| target_diff.symbol_diff(r)) {
        Some(tsym) if tsym.instructions.len() == bsym.instructions.len() => {
            for (l, r) in bsym.instructions.iter().zip(tsym.instructions.iter()) {
                render_row(&mut listing, l, r);
            }
        }
        _ => {
            for l in &bsym.instructions {
                render_row(&mut listing, l, l);
            }
        }
    }

    Ok(Some(ObjdiffResult { match_percent, listing }))
}

fn find_symbol<'a>(diff: &'a ObjDiff, obj: &ObjInfo, mangled: &str) -> Option<&'a ObjSymbolDiff> {
    diff.sections.iter().flat_map(|s| s.symbols.iter()).find(|sd| {
        let sym = &obj.sections[sd.symbol_ref.section_idx].symbols[sd.symbol_ref.symbol_idx];
        sym.name == mangled
    })
}

fn text(d: &ObjInsDiff) -> &str {
    d.ins.as_ref().map(|i| i.formatted.as_str()).unwrap_or("")
}

/// `l` = base row, `r` = the aligned target row. Markers: `  ` equal, `~`
/// op/arg/replace mismatch (base -> target), `-` base-only, `+` target-only.
fn render_row(out: &mut String, l: &ObjInsDiff, r: &ObjInsDiff) {
    match l.kind {
        ObjInsDiffKind::None => {
            let _ = writeln!(out, "  {}", text(l));
        }
        ObjInsDiffKind::Delete => {
            let _ = writeln!(out, "- {}", text(l));
        }
        ObjInsDiffKind::Insert => {
            let _ = writeln!(out, "+ {}", text(r));
        }
        ObjInsDiffKind::Replace | ObjInsDiffKind::OpMismatch | ObjInsDiffKind::ArgMismatch => {
            let _ = writeln!(out, "~ {:<30} -> {}", text(l), text(r));
        }
    }
}
