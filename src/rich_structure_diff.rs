//! Base↔target *structure* diff: the statement-skeleton analog of the assembly
//! [`crate::rich_diff`]. Where that view aligns instruction text, this one aligns
//! the per-statement `(offset, size, line)` skeleton — the cheap structural
//! signal a matcher reads before generating code — so an agent sees exactly which
//! statement sizes diverge and where a statement is present on one side only.
//!
//! Pipeline mirrors the carcass derivation in [`crate::gen_sources`]:
//!   1. [`structure_rows`] turns a [`FunctionEntry`] into BODY rows, skipping the
//!      synthetic first/last frame braces and collapsing each source-line GAP
//!      (comment/blank/`#ifdef` runs) into a single [`Row::Empty`].
//!   2. [`diff_structure`] LCS-aligns the two row streams keyed on statement SIZE
//!      (empties match empties), then merges adjacent delete+insert runs into
//!      `Changed` rows (same statement, different size).
//!   3. [`render_structure_diff`] prints the aligned rows side-by-side, target on
//!      the left (matching the `target:` / `base:` header order), divergences
//!      tagged.
//!
//! v1 carries no `[n]` block-depth marker: the rich index records no block-depth
//! field, so depth is simply unavailable here (unlike the carcass, which reads it
//! straight from the PDB line program).

use std::fmt::Write as _;

use crate::rich_context::FunctionEntry;

/// One BODY row of a single side's statement skeleton. `Empty` is a collapsed
/// source-line gap (the carcass `<0>..<n>` run squashed to one marker).
pub enum Row<'a> {
    Stmt {
        off: u32,
        size: u32,
        line: u32,
        source: Option<&'a str>,
    },
    Empty,
}

/// Derive the BODY row sequence for one side, mirroring the carcass logic in
/// `gen_sources`: skip the first+last synthetic frame-brace statements, and
/// collapse every source-line gap between consecutive body statements (and before
/// the closing brace) into a single [`Row::Empty`].
pub fn structure_rows(f: &FunctionEntry) -> Vec<Row<'_>> {
    // Body-less functions carry only the frame braces — no body rows at all.
    if f.is_body_less() {
        return Vec::new();
    }

    let stmts = &f.statements;
    let len = stmts.len();
    let mut rows = Vec::new();

    // Track the next expected source line; a body statement whose line jumps ahead
    // signals a gap (comments/blanks/#ifdef) collapsed to one Empty.
    let mut next_line = stmts[0].line;

    for (i, s) in stmts.iter().enumerate() {
        // Skip the synthetic frame braces (same rule the carcass uses).
        if i == 0 || i == len - 1 {
            continue;
        }
        if s.line > next_line {
            rows.push(Row::Empty);
        }
        next_line = s.line + 1;
        rows.push(Row::Stmt {
            off: s.off,
            size: s.size,
            line: s.line,
            source: s.source.as_deref(),
        });
    }

    // A trailing gap between the last body statement and the closing frame brace.
    if let Some(last) = stmts.last() {
        if last.line > next_line {
            rows.push(Row::Empty);
        }
    }

    // Empty rows at the very start or end are just blank-line/comment/#ifdef gaps at the
    // function's top or bottom - not structural signal. Trim them so the diff begins and
    // ends on a real statement (and we don't emit boundary `EMPTY only ...` rows).
    while matches!(rows.first(), Some(Row::Empty)) {
        rows.remove(0);
    }
    while matches!(rows.last(), Some(Row::Empty)) {
        rows.pop();
    }

    rows
}

/// One aligned row of the structure diff. `base`/`target` carry the per-side
/// statement when present.
pub enum StructRow<'a> {
    /// Statement present on both sides with equal size.
    Equal {
        base: &'a Row<'a>,
        target: &'a Row<'a>,
    },
    /// Statement present on both sides but with a different size (a real
    /// structural divergence the matcher must close).
    Changed {
        base: &'a Row<'a>,
        target: &'a Row<'a>,
    },
    /// Statement present on the target only (missing in base).
    OnlyTarget { stmt: &'a Row<'a> },
    /// Statement present on the base only (extra).
    OnlyBase { stmt: &'a Row<'a> },
    /// Collapsed empty-line run present on both sides.
    EmptyEqual,
    /// Collapsed empty-line run present on the target only.
    EmptyOnlyTarget,
    /// Collapsed empty-line run present on the base only.
    EmptyOnlyBase,
}

fn is_stmt(r: &Row) -> bool {
    matches!(r, Row::Stmt { .. })
}

/// LCS alignment key for a row: empties match empties; statements match when
/// their SIZE is equal (the cheap structural identity the matcher compares).
fn row_key(r: &Row) -> String {
    match r {
        Row::Empty => "E".to_string(),
        Row::Stmt { size, .. } => format!("S{size}"),
    }
}

/// Internal op stream from the LCS backtrack (same shape as `rich_diff::diff`).
enum RawOp {
    Equal {
        base: usize,
        target: usize,
    },
    /// Base-only (delete to reach target).
    Delete {
        base: usize,
    },
    /// Target-only (insert to reach target).
    Insert {
        target: usize,
    },
}

/// Align the two BODY row streams (base = left, target = right, mirroring
/// `render_unified`'s `--- base / +++ target`) by statement size via LCS, then
/// merge adjacent delete+insert runs into `Changed` rows.
pub fn diff_structure<'a>(
    base_rows: &'a [Row<'a>],
    target_rows: &'a [Row<'a>],
) -> Vec<StructRow<'a>> {
    let b: Vec<String> = base_rows.iter().map(row_key).collect();
    let t: Vec<String> = target_rows.iter().map(row_key).collect();
    let (n, m) = (b.len(), t.len());

    // dp[i][j] = LCS length of b[i..] and t[j..] — copied from rich_diff::diff.
    let mut dp = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            dp[i][j] = if b[i] == t[j] {
                dp[i + 1][j + 1] + 1
            } else {
                dp[i + 1][j].max(dp[i][j + 1])
            };
        }
    }

    let mut ops = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if b[i] == t[j] {
            ops.push(RawOp::Equal { base: i, target: j });
            i += 1;
            j += 1;
        } else if dp[i + 1][j] >= dp[i][j + 1] {
            ops.push(RawOp::Delete { base: i });
            i += 1;
        } else {
            ops.push(RawOp::Insert { target: j });
            j += 1;
        }
    }
    while i < n {
        ops.push(RawOp::Delete { base: i });
        i += 1;
    }
    while j < m {
        ops.push(RawOp::Insert { target: j });
        j += 1;
    }

    // POST-PROCESS: a run of Deletes immediately followed by a run of Inserts is
    // the same statement appearing on both sides with a different size; pair them
    // in order into `Changed` rows. Leftovers stay one-sided (a real quantity
    // divergence). Empty rows are never merged.
    let mut out = Vec::new();
    let mut k = 0;
    while k < ops.len() {
        match &ops[k] {
            RawOp::Equal { base, target } => {
                let (br, tr) = (&base_rows[*base], &target_rows[*target]);
                // Equal LCS key ⇒ same size (or both Empty).
                out.push(match (br, tr) {
                    (Row::Empty, Row::Empty) => StructRow::EmptyEqual,
                    _ => StructRow::Equal {
                        base: br,
                        target: tr,
                    },
                });
                k += 1;
            }
            RawOp::Delete { .. } => {
                // Collect the contiguous Delete run, then the following Insert run.
                let del_start = k;
                while k < ops.len() && matches!(ops[k], RawOp::Delete { .. }) {
                    k += 1;
                }
                let del_end = k;
                let ins_start = k;
                while k < ops.len() && matches!(ops[k], RawOp::Insert { .. }) {
                    k += 1;
                }
                let ins_end = k;

                // Statement rows of each run, in order — only these may pair into
                // `Changed`. Empty rows are never merged; they are emitted as the
                // one-sided empty variant in place.
                let del_stmts: Vec<&Row> = ops[del_start..del_end]
                    .iter()
                    .map(|o| {
                        let RawOp::Delete { base } = o else {
                            unreachable!()
                        };
                        &base_rows[*base]
                    })
                    .collect();
                let ins_stmts: Vec<&Row> = ops[ins_start..ins_end]
                    .iter()
                    .map(|o| {
                        let RawOp::Insert { target } = o else {
                            unreachable!()
                        };
                        &target_rows[*target]
                    })
                    .collect();

                let del_count = del_stmts.iter().filter(|r| is_stmt(r)).count();
                let ins_count = ins_stmts.iter().filter(|r| is_stmt(r)).count();
                let pairs = del_count.min(ins_count);

                // Emit the base side: each base Stmt up to `pairs` becomes a
                // `Changed` (paired with the matching target Stmt); leftover base
                // Stmts are only-in-base; Empties pass through one-sided.
                let mut ins_iter = ins_stmts.iter().filter(|r| is_stmt(r));
                let mut paired = 0;
                for br in &del_stmts {
                    match br {
                        Row::Empty => out.push(StructRow::EmptyOnlyBase),
                        Row::Stmt { .. } if paired < pairs => {
                            let tr = ins_iter.next().unwrap();
                            paired += 1;
                            out.push(StructRow::Changed {
                                base: br,
                                target: tr,
                            });
                        }
                        Row::Stmt { .. } => out.push(one_side(br, Side::Base)),
                    }
                }
                // Remaining target rows: target Stmts beyond `pairs` are
                // only-in-target; target Empties pass through one-sided.
                let mut consumed = 0;
                for tr in &ins_stmts {
                    match tr {
                        Row::Empty => out.push(StructRow::EmptyOnlyTarget),
                        Row::Stmt { .. } if consumed < pairs => consumed += 1,
                        Row::Stmt { .. } => out.push(one_side(tr, Side::Target)),
                    }
                }
            }
            RawOp::Insert { target } => {
                // An Insert run with no preceding Delete run (handled above).
                out.push(one_side(&target_rows[*target], Side::Target));
                k += 1;
            }
        }
    }

    out
}

enum Side {
    Base,
    Target,
}

/// A one-sided row: an Empty becomes the empty-only variant, a Stmt the
/// only-base/only-target variant.
fn one_side<'a>(r: &'a Row<'a>, side: Side) -> StructRow<'a> {
    match (r, side) {
        (Row::Empty, Side::Base) => StructRow::EmptyOnlyBase,
        (Row::Empty, Side::Target) => StructRow::EmptyOnlyTarget,
        (Row::Stmt { .. }, Side::Base) => StructRow::OnlyBase { stmt: r },
        (Row::Stmt { .. }, Side::Target) => StructRow::OnlyTarget { stmt: r },
    }
}

/// Render one side's cell: `0x{off:02x}  <0x{size:x}>  {label}`, where the target
/// label is `L{line}` (target carries no source) and the base label is its source
/// text when present, else `L{line}`. An absent side renders as `--`.
fn cell(r: Option<&Row>) -> String {
    match r {
        None => "--".to_string(),
        Some(Row::Empty) => "<0>".to_string(),
        Some(Row::Stmt {
            off,
            size,
            line,
            source,
        }) => {
            let label = match source {
                Some(src) => (*src).to_string(),
                None => format!("L{line}"),
            };
            format!("0x{off:02x}  <0x{size:x}>  {label}")
        }
    }
}

/// Statement size of a row (0 for Empty — only used to tag Changed rows, which
/// are always Stmt/Stmt pairs).
fn row_size(r: &Row) -> u32 {
    match r {
        Row::Stmt { size, .. } => *size,
        Row::Empty => 0,
    }
}

/// Render the full side-by-side structure diff. Target is the left column (to
/// match the `target:` / `base:` header order); each divergence carries a
/// trailing tag. When `condensed`, collapse aligned-equal runs to `.. same ..`
/// and emit only the divergence rows in a compact one-line form.
pub fn render_structure_diff(
    base: &FunctionEntry,
    target: &FunctionEntry,
    condensed: bool,
) -> String {
    if condensed {
        return render_condensed(base, target);
    }
    let base_rows = structure_rows(base);
    let target_rows = structure_rows(target);
    let rows = diff_structure(&base_rows, &target_rows);

    let mut out = String::new();
    // Header 1: each side's real address, so the agent can locate the function.
    let _ = writeln!(
        out,
        "target: 0x{:x}            base: 0x{:x}",
        target.rva, base.rva
    );
    // Header 2: function name + per-side body-statement counts.
    let _ = writeln!(
        out,
        "; {} ; target {} stmts / base {} stmts",
        target.name,
        target_rows.len(),
        base_rows.len()
    );

    // Column width for the target cell so the base column lines up. Computed over
    // the rendered target cells; falls back to a sane minimum.
    let width = rows
        .iter()
        .map(|r| {
            cell(match r {
                StructRow::Equal { target, .. } | StructRow::Changed { target, .. } => Some(target),
                StructRow::OnlyTarget { stmt } => Some(stmt),
                StructRow::EmptyEqual | StructRow::EmptyOnlyTarget => Some(&Row::Empty),
                _ => None,
            })
            .len()
        })
        .max()
        .unwrap_or(2)
        .max(2);

    let (mut aligned, mut size_diffs, mut quantity_diffs) = (0usize, 0usize, 0usize);

    for r in &rows {
        let (target_cell, base_cell, tag) = match r {
            StructRow::Equal { base, target } => {
                aligned += 1;
                (cell(Some(target)), cell(Some(base)), String::new())
            }
            StructRow::Changed { base, target } => {
                size_diffs += 1;
                let tag = format!(
                    "  <- SIZE  (target 0x{:x} vs base 0x{:x})",
                    row_size(target),
                    row_size(base)
                );
                (cell(Some(target)), cell(Some(base)), tag)
            }
            StructRow::OnlyTarget { stmt } => {
                quantity_diffs += 1;
                (
                    cell(Some(stmt)),
                    cell(None),
                    "  <- only in target (missing in base)".to_string(),
                )
            }
            StructRow::OnlyBase { stmt } => {
                quantity_diffs += 1;
                (
                    cell(None),
                    cell(Some(stmt)),
                    "  <- only in base (extra)".to_string(),
                )
            }
            StructRow::EmptyEqual => {
                aligned += 1;
                (
                    cell(Some(&Row::Empty)),
                    cell(Some(&Row::Empty)),
                    String::new(),
                )
            }
            StructRow::EmptyOnlyTarget => {
                quantity_diffs += 1;
                (
                    cell(Some(&Row::Empty)),
                    cell(None),
                    "  <- empty-line run only on target".to_string(),
                )
            }
            StructRow::EmptyOnlyBase => {
                quantity_diffs += 1;
                (
                    cell(None),
                    cell(Some(&Row::Empty)),
                    "  <- empty-line run only on base".to_string(),
                )
            }
        };

        let _ = writeln!(out, "{target_cell:<width$}    {base_cell}{tag}");
    }

    let _ = writeln!(
        out,
        "; aligned {aligned}, size-diffs {size_diffs}, quantity-diffs {quantity_diffs}"
    );
    out
}

/// Compact per-side `0x{off:03x} <0x{size:x}>` cell for the condensed view; an
/// absent side (and an Empty, which has no real offset) renders as a padded `--`.
fn compact_cell(r: Option<&Row>) -> String {
    match r {
        Some(Row::Stmt { off, size, .. }) => format!("0x{off:03x} <0x{size:x}>"),
        // An Empty (collapsed source-line gap) has no offset; show the `<0>` marker
        // on the side that HAS it so a one-sided empty run is visible, not `--`.
        Some(Row::Empty) => format!("{:<11}", "<0>"),
        _ => format!("{:<11}", "--"),
    }
}

/// The source-statement text for a divergence row: the base `source` when
/// present, else `L{line}` from whichever side carries a statement.
fn stmt_text(base: Option<&Row>, target: Option<&Row>) -> String {
    if let Some(Row::Stmt {
        source: Some(src), ..
    }) = base
    {
        return (*src).to_string();
    }
    for r in [base, target].into_iter().flatten() {
        if let Row::Stmt { line, .. } = r {
            return format!("L{line}");
        }
    }
    String::new()
}

/// Condensed structure diff: header + summary unchanged, equal runs collapsed to
/// a single `.. same ..`, and only the divergence rows emitted in compact form.
/// After the first size divergence the per-side offsets DRIFT apart (they
/// accumulate the size delta) — that is expected; we keep showing raw per-side
/// offsets without re-anchoring.
fn render_condensed(base: &FunctionEntry, target: &FunctionEntry) -> String {
    let base_rows = structure_rows(base);
    let target_rows = structure_rows(target);
    let rows = diff_structure(&base_rows, &target_rows);

    // Count only REAL statements (`Row::Stmt`) for the header. Blank-line gaps
    // (`Row::Empty`) are not statements; counting them makes an empty-line-only
    // attribution look like a lost/gained statement (the "target N / base N-1"
    // confusion). Real-statement totals are equal whenever no real stmt diverges.
    let target_stmts = target_rows
        .iter()
        .filter(|r| matches!(r, Row::Stmt { .. }))
        .count();
    let base_stmts = base_rows
        .iter()
        .filter(|r| matches!(r, Row::Stmt { .. }))
        .count();

    let mut out = String::new();
    let _ = writeln!(
        out,
        "target: 0x{:x}            base: 0x{:x}",
        target.rva, base.rva
    );
    let _ = writeln!(
        out,
        "; {} ; target {} stmts / base {} stmts",
        target.name, target_stmts, base_stmts
    );

    let (mut aligned, mut size_diffs, mut quantity_diffs, mut blank_gaps) =
        (0usize, 0usize, 0usize, 0usize);
    // Monotonic REAL-statement row index (1-based), printed as an `NN:` prefix so
    // each divergence row is locatable and a collapsed `.. same ..` run is bracketed
    // by the index it resumes at. One shared counter (not per-side) so it never
    // repeats or goes backwards at an ONLY base/target divergence. Blank rows do not
    // advance it.
    let mut n = 0usize;
    // Collapse a maximal run of aligned-equal rows into one `.. same ..` marker.
    let mut pending_same = false;
    let flush_same = |out: &mut String, pending: &mut bool| {
        if *pending {
            let _ = writeln!(out, ".. same ..");
            *pending = false;
        }
    };

    for r in &rows {
        match r {
            StructRow::Equal { .. } => {
                aligned += 1;
                n += 1;
                pending_same = true;
                continue;
            }
            // Blank line on both sides: part of the aligned run, not a statement.
            StructRow::EmptyEqual => {
                pending_same = true;
                continue;
            }
            StructRow::Changed { base, target } => {
                n += 1;
                size_diffs += 1;
                flush_same(&mut out, &mut pending_same);
                let _ = writeln!(
                    out,
                    "{:>3}: {} | {} | {}   SIZE",
                                        n,
                    compact_cell(Some(target)),
                    compact_cell(Some(base)),
                    stmt_text(Some(base), Some(target)),
                );
            }
            StructRow::OnlyBase { stmt } => {
                n += 1;
                quantity_diffs += 1;
                flush_same(&mut out, &mut pending_same);
                let _ = writeln!(
                    out,
                    "{:>3}: {} | {} | {}   ONLY base",
                    n,
                    compact_cell(None),
                    compact_cell(Some(stmt)),
                    stmt_text(Some(stmt), None),
                );
            }
            StructRow::OnlyTarget { stmt } => {
                n += 1;
                quantity_diffs += 1;
                flush_same(&mut out, &mut pending_same);
                let _ = writeln!(
                    out,
                    "{:>3}: {} | {} | {}   ONLY target",
                    n,
                    compact_cell(Some(stmt)),
                    compact_cell(None),
                    stmt_text(None, Some(stmt)),
                );
            }
            // Blank-line-only attribution gap: NOT a real divergence. Suppress the
            // row (it was pure noise); tally it as a `blank-gaps` for transparency.
            StructRow::EmptyOnlyBase | StructRow::EmptyOnlyTarget => {
                blank_gaps += 1;
            }
        }
    }
    flush_same(&mut out, &mut pending_same);

    let _ = writeln!(
        out,
        "; aligned {aligned}, size-diffs {size_diffs}, quantity-diffs {quantity_diffs}, blank-gaps {blank_gaps}"
    );
    out
}
