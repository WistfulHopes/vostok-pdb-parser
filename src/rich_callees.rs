//! "callees" view: the functions a given function calls, resolved to their full
//! signatures. Call targets are read out of the function's own (already
//! symbol-resolved) instruction text; resolution is one streaming pass over an
//! index. Lets the agent see what a body depends on without a second round-trip,
//! and match callees/forced-inline helpers before their callers.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::BufRead as _;
use std::path::Path;

use crate::rich_context::FunctionEntry;

/// Distinct `call` targets that look like named symbols (not registers, memory
/// operands, local labels, or raw addresses), in first-seen order.
pub fn extract(f: &FunctionEntry) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for insn in &f.instructions {
        let Some(target) = insn.text.strip_prefix("call ").map(str::trim) else {
            continue;
        };
        if is_named_symbol(target) && seen.insert(target.to_string()) {
            out.push(target.to_string());
        }
    }
    out
}

fn is_named_symbol(s: &str) -> bool {
    let Some(&first) = s.as_bytes().first() else {
        return false;
    };
    // Exclude local labels (`.1`), memory operands (`[eax+4]`) and raw addresses.
    if first == b'.' || first == b'[' || first.is_ascii_digit() {
        return false;
    }
    if is_register(s) {
        return false;
    }
    // A qualified C++ name (`a::b`) or a bare identifier (`memcpy`); never spaces
    // unless it is a qualified name (templates/operators can contain them).
    matches!(first, b'a'..=b'z' | b'A'..=b'Z' | b'_' | b'?')
        && (s.contains("::") || !s.contains(' '))
}

fn is_register(s: &str) -> bool {
    const REGS: &[&str] = &[
        "eax", "ebx", "ecx", "edx", "esi", "edi", "ebp", "esp", "ax", "bx", "cx", "dx", "al", "bl",
        "cl", "dl", "ah", "bh", "ch", "dh",
    ];
    REGS.contains(&s)
}

/// Resolve each callee to the index entries whose signature it names. Matches
/// `<callee>(` (call to a function), falling back to a plain substring for the
/// no-paren oddities. One pass over the index.
pub fn resolve(index: &Path, callees: &[String]) -> crate::Result<BTreeMap<String, Vec<String>>> {
    let mut map: BTreeMap<String, Vec<String>> =
        callees.iter().map(|c| (c.clone(), Vec::new())).collect();
    if callees.is_empty() {
        return Ok(map);
    }

    let reader = std::io::BufReader::new(std::fs::File::open(index)?);
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let entry: FunctionEntry = serde_json::from_str(&line)?;
        for callee in callees {
            let hit = entry.name.contains(&format!("{callee}("))
                || (!entry.name.contains('(') && entry.name.contains(callee.as_str()));
            if hit {
                map.get_mut(callee).unwrap().push(entry.name.clone());
            }
        }
    }
    Ok(map)
}

/// Render the callee list, each with its resolved signature(s) or `(unresolved)`.
pub fn render(
    f: &FunctionEntry,
    callees: &[String],
    resolved: &BTreeMap<String, Vec<String>>,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}:", f.name);
    let _ = writeln!(out, "; callees ({})", callees.len());
    for callee in callees {
        match resolved.get(callee) {
            Some(sigs) if !sigs.is_empty() => {
                for sig in sigs {
                    let _ = writeln!(out, "  {callee}\t-> {sig}");
                }
            }
            _ => {
                let _ = writeln!(out, "  {callee}\t-> (unresolved)");
            }
        }
    }
    out
}
