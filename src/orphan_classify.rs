//! Git-tracked orphan-classification database.
//!
//! A JSONL file (`orphan-classifications.jsonl`) that maps mangled function
//! names to a human/agent-assigned status, so agents can distinguish
//! *unimplemented* orphans from *unmatchable* ones (inline-only, template
//! stubs, system headers).
//!
//! ```jsonl
//! {"mangled":"?Free@Scaleform@@UAEXPAXII@Z","status":"unmatchable","reason":"Scaleform SDK","reviewed_at":"...","reviewed_by":"cli"}
//! {"mangled":"?tick@bullet@@QAEXI@Z","status":"todo","reason":null,"reviewed_at":null,"reviewed_by":null}
//! ```

use std::collections::HashMap;
use std::io::BufRead;
use std::io::Write;
use std::path::Path;

use anyhow::Context;
use serde::Deserialize;
use serde::Serialize;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Clone, Serialize, Deserialize)]
pub struct ClassEntry {
    pub mangled: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewed_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewed_by: Option<String>,
}

/// Lightweight mark attached to a `FuncEntry` during query — avoids dragging
/// the full `ClassEntry` metadata into every output row.
#[derive(Clone, Serialize)]
pub struct ClassMark {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

// ---------------------------------------------------------------------------
// I/O
// ---------------------------------------------------------------------------

/// Load the full database into a `mangled → ClassEntry` map.
/// Returns an empty map when the file does not exist.
pub fn load(path: &Path) -> anyhow::Result<HashMap<String, ClassEntry>> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(e).context("opening classification db"),
    };
    let reader = std::io::BufReader::new(file);
    let mut map = HashMap::new();
    for line in reader.lines() {
        let line = line.context("reading classification line")?;
        if line.trim().is_empty() {
            continue;
        }
        let entry: ClassEntry =
            serde_json::from_str(&line).context("deserialising classification entry")?;
        map.insert(entry.mangled.clone(), entry);
    }
    Ok(map)
}

/// Overwrite the file with the current map contents, sorted by mangled name.
pub fn save(path: &Path, db: &HashMap<String, ClassEntry>) -> anyhow::Result<()> {
    let mut entries: Vec<&ClassEntry> = db.values().collect();
    entries.sort_by(|a, b| a.mangled.cmp(&b.mangled));

    let file = std::fs::File::create(path)
        .with_context(|| format!("creating {}", path.display()))?;
    let mut writer = std::io::BufWriter::new(file);
    for e in &entries {
        serde_json::to_writer(&mut writer, e)?;
        writer.write_all(b"\n")?;
    }
    writer.flush()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Mutations
// ---------------------------------------------------------------------------

/// Insert or update one entry, then save.
pub fn upsert(
    path: &Path,
    mangled: &str,
    status: &str,
    reason: Option<&str>,
) -> anyhow::Result<()> {
    let mut db = load(path)?;
    let now = chrono::Utc::now().to_rfc3339();
    let reviewer = std::env::var("USER").unwrap_or_else(|_| "?".into());
    db.insert(
        mangled.to_string(),
        ClassEntry {
            mangled: mangled.to_string(),
            status: status.to_string(),
            reason: reason.map(|s| s.to_string()),
            reviewed_at: Some(now),
            reviewed_by: Some(reviewer),
        },
    );
    save(path, &db)
}

/// Remove one entry by mangled name, then save.  No-op if not present.
pub fn remove(path: &Path, mangled: &str) -> anyhow::Result<()> {
    let mut db = load(path)?;
    db.remove(mangled);
    save(path, &db)
}
