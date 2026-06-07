//! Query the rich-context function index produced by `pdb_rich_context --out`.
//!
//! The index (`<out>/index.jsonl`) holds one JSON [`FunctionEntry`] per line. A
//! query streams it, matches on the function signature (case-insensitive
//! substring) and/or an exact RVA, and returns the pre-rendered block — no PDB
//! parsing, so an agent loop gets a function immediately. This is the "query on
//! top of" the complete rebuild.

use std::io::BufRead as _;
use std::path::Path;

use crate::rich_context::FunctionEntry;

#[derive(Default)]
pub struct Query<'a> {
    /// Case-insensitive substring matched against the function signature.
    pub name: Option<&'a str>,
    /// Exact function RVA (image-relative).
    pub rva: Option<u32>,
    /// Exact decorated (mangled) COFF symbol. Identical across the two PDBs for
    /// the same function, so it is the precise base↔target join key — unlike the
    /// demangled `name`, which can differ (e.g. `const` on by-value params).
    pub mangled: Option<&'a str>,
}

/// Stream `index.jsonl`, returning entries that match `query`, sorted by
/// (file, rva). With no filter set, returns every entry (useful for listing).
pub fn search(index_path: &Path, query: &Query) -> crate::Result<Vec<FunctionEntry>> {
    let file = std::fs::File::open(index_path)?;
    let reader = std::io::BufReader::new(file);

    let needle = query.name.map(str::to_lowercase);

    let mut hits = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let entry: FunctionEntry = serde_json::from_str(&line)?;

        if query.rva.is_some_and(|rva| entry.rva != rva) {
            continue;
        }
        if query.mangled.is_some_and(|m| entry.mangled != m) {
            continue;
        }
        if let Some(needle) = &needle {
            if !entry.name.to_lowercase().contains(needle) {
                continue;
            }
        }
        hits.push(entry);
    }

    hits.sort_by(|a, b| a.file.cmp(&b.file).then(a.rva.cmp(&b.rva)));
    Ok(hits)
}
