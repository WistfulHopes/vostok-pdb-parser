//! Parse and query objdiff `report.json` for function matching statistics.
//!
//! The report is a flat JSON file with top-level aggregate `measures` and a
//! `units` array — one entry per compilation unit (`.obj` file). Each unit
//! carries its own measures and a `functions` list. Functions are joined
//! across base/target by their **mangled** name (report `name` == index
//! `mangled`); `fuzzy_match_percent` is `None` when a function exists on
//! only one side.

use std::collections::HashMap;
use std::io::BufRead;
use std::path::Path;

use anyhow::Context;
use serde::Deserialize;
use serde::Serialize;

// ---------------------------------------------------------------------------
// Hex formatting for u64/u32 fields in JSON output
// ---------------------------------------------------------------------------

fn serialize_hex_u64<S: serde::Serializer>(val: &u64, s: S) -> Result<S::Ok, S::Error> {
    s.collect_str(&format_args!("0x{val:x}"))
}

fn serialize_hex_u32<S: serde::Serializer>(val: &u32, s: S) -> Result<S::Ok, S::Error> {
    s.collect_str(&format_args!("0x{val:x}"))
}

// ---------------------------------------------------------------------------
// Raw report.json deserialization types
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ReportJson {
    #[serde(default)]
    measures: TopMeasures,
    #[serde(default)]
    units: Vec<UnitJson>,
}

#[derive(Serialize, Deserialize, Default)]
pub struct TopMeasures {
    #[serde(default)]
    pub fuzzy_match_percent: Option<f64>,
    #[serde(default)]
    pub total_functions: Option<u64>,
    #[serde(default)]
    pub matched_functions: Option<u64>,
    #[serde(default)]
    pub matched_functions_percent: Option<f64>,
    #[serde(default)]
    pub total_code: Option<String>,
    #[serde(default)]
    pub matched_code: Option<String>,
    #[serde(default)]
    pub matched_code_percent: Option<f64>,
    #[serde(default)]
    pub total_units: Option<u64>,
}

#[derive(Deserialize)]
struct UnitJson {
    name: String,
    #[serde(default)]
    functions: Vec<FuncJson>,
}

#[derive(Deserialize)]
struct FuncJson {
    name: String,
    size: String,
    #[serde(default)]
    fuzzy_match_percent: Option<f64>,
    address: String,
    #[serde(default)]
    metadata: FuncMetadata,
}

#[derive(Deserialize, Default)]
struct FuncMetadata {
    #[serde(default)]
    demangled_name: Option<String>,
}

// ---------------------------------------------------------------------------
// Public output types
// ---------------------------------------------------------------------------

/// One function, flattened out of its unit — the unit of query.
#[derive(Clone, Serialize)]
pub struct FuncEntry {
    /// Mangled C++ symbol name — the join key shared with `index.jsonl`.
    pub name: String,
    /// Demangled signature. Populated from report.json `metadata.demangled_name`
    /// initially; overwritten with the cleaner index `name` when enriched.
    pub demangled: Option<String>,
    /// Parent compilation-unit name (e.g. `vostok/game_core/sources/weapon.cpp`).
    pub unit: String,
    /// Function byte length.
    #[serde(serialize_with = "serialize_hex_u64")]
    pub size: u64,
    /// Match percentage; `None` when the function exists on only one side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fuzzy_match_percent: Option<f64>,
    /// Byte offset of this function within its unit (NOT an RVA).
    #[serde(serialize_with = "serialize_hex_u64")]
    pub address: u64,
    /// Fields filled from `index.jsonl` when `--target-index` is supplied.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enriched: Option<IndexEnrichment>,
    /// Orphan classification from `orphan-classifications.jsonl`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classification: Option<ClassMark>,
}

/// Fields cross-referenced from `index.jsonl` by mangled name.
#[derive(Clone, Serialize)]
pub struct IndexEnrichment {
    #[serde(serialize_with = "serialize_hex_u32")]
    pub rva: u32,
    pub file: String,
}

/// Lightweight orphan-classification mark attached to a `FuncEntry`.
#[derive(Clone, Serialize)]
pub struct ClassMark {
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A lightweight index entry — only the fields needed for enrichment and
/// cross-reference (avoids deserialising per-function instructions/statements).
#[derive(Clone, Serialize, Deserialize)]
pub struct IndexEntry {
    pub mangled: String,
    pub name: String,
    #[serde(serialize_with = "serialize_hex_u32")]
    pub rva: u32,
    #[serde(serialize_with = "serialize_hex_u32")]
    pub size: u32,
    pub file: String,
}

// ---------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------

pub struct FuncFilter<'a> {
    pub unit_pattern: Option<&'a str>,
    pub min_percent: Option<f64>,
    pub max_percent: Option<f64>,
    pub matched_only: bool,
    pub min_size: Option<u64>,
    pub status_filter: Option<&'a str>,
    pub limit: Option<usize>,
    pub sort: SortField,
    pub order: SortOrder,
}

impl<'a> Default for FuncFilter<'a> {
    fn default() -> Self {
        Self {
            unit_pattern: None,
            min_percent: None,
            max_percent: None,
            matched_only: false,
            min_size: None,
            status_filter: None,
            limit: None,
            sort: SortField::default(),
            order: SortOrder::default(),
        }
    }
}

#[derive(Clone, Copy, Default)]
pub enum SortField {
    #[default]
    Percent,
    Size,
    Name,
}

#[derive(Clone, Copy, Default)]
pub enum SortOrder {
    #[default]
    Asc,
    Desc,
}

// ---------------------------------------------------------------------------
// Summary output
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct Summary {
    pub top_level: TopMeasures,
    pub buckets: HashMap<String, BucketStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by_unit: Option<Vec<UnitSummary>>,
}

#[derive(Serialize)]
pub struct BucketStats {
    pub count: usize,
    #[serde(serialize_with = "serialize_hex_u64")]
    pub code_bytes: u64,
}

#[derive(Serialize)]
pub struct UnitSummary {
    pub name: String,
    pub total_functions: Option<u64>,
    pub matched_functions: Option<u64>,
    pub total_code: Option<String>,
    pub matched_code: Option<String>,
    pub fuzzy_match_percent: Option<f64>,
}

// ---------------------------------------------------------------------------
// Orphan output
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct OrphanOutput {
    pub report: Vec<FuncEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub index_only: Option<IndexOnlyOrphans>,
}

#[derive(Serialize)]
pub struct IndexOnlyOrphans {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base: Option<Vec<IndexEntry>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<Vec<IndexEntry>>,
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

fn parse_json_string_u64(s: &str) -> anyhow::Result<u64> {
    s.parse().with_context(|| format!("failed to parse '{}' as u64", s))
}

/// Load and flatten `report.json`.
pub fn load_report(path: &Path) -> anyhow::Result<Vec<FuncEntry>> {
    let file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let report: ReportJson =
        serde_json::from_reader(file).context("deserialising report.json")?;

    let mut entries = Vec::new();
    for unit in &report.units {
        for func in &unit.functions {
            entries.push(FuncEntry {
                name: func.name.clone(),
                demangled: func.metadata.demangled_name.clone(),
                unit: unit.name.clone(),
                size: parse_json_string_u64(&func.size)?,
                fuzzy_match_percent: func.fuzzy_match_percent,
                address: parse_json_string_u64(&func.address)?,
                enriched: None,
                classification: None,
            });
        }
    }
    Ok(entries)
}

/// Load top-level measures (without parsing every function).
pub fn load_top_measures(path: &Path) -> anyhow::Result<TopMeasures> {
    let file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let report: ReportJson =
        serde_json::from_reader(file).context("deserialising report.json")?;
    Ok(report.measures)
}

// ---------------------------------------------------------------------------
// Filtering / sorting
// ---------------------------------------------------------------------------

/// Filter, sort, and limit a flat function list.
pub fn filter_functions<'a>(
    functions: &'a [FuncEntry],
    filter: &FuncFilter,
) -> Vec<&'a FuncEntry> {
    let mut result: Vec<&FuncEntry> = functions
        .iter()
        .filter(|f| {
            if filter.matched_only && f.fuzzy_match_percent.is_none() {
                return false;
            }
            if let Some(p) = filter.min_percent {
                match f.fuzzy_match_percent {
                    Some(fp) if fp >= p => {}
                    _ => return false,
                }
            }
            if let Some(p) = filter.max_percent {
                match f.fuzzy_match_percent {
                    Some(fp) if fp <= p => {}
                    _ => return false,
                }
            }
            if let Some(s) = filter.min_size {
                if f.size < s {
                    return false;
                }
            }
            if let Some(pat) = &filter.unit_pattern {
                if !f.unit.contains(pat) {
                    return false;
                }
            }
            if let Some(st) = &filter.status_filter {
                match &f.classification {
                    Some(c) if c.status == *st => {}
                    _ => return false,
                }
            }
            true
        })
        .collect();

    result.sort_by(|a, b| match filter.sort {
        SortField::Percent => {
            let pa = a.fuzzy_match_percent.unwrap_or(f64::NEG_INFINITY);
            let pb = b.fuzzy_match_percent.unwrap_or(f64::NEG_INFINITY);
            pa.partial_cmp(&pb).unwrap_or(std::cmp::Ordering::Equal)
        }
        SortField::Size => a.size.cmp(&b.size),
        SortField::Name => a.name.cmp(&b.name),
    });

    if matches!(filter.order, SortOrder::Desc) {
        result.reverse();
    }

    if let Some(limit) = filter.limit {
        result.truncate(limit);
    }

    result
}

// ---------------------------------------------------------------------------
// Orphans
// ---------------------------------------------------------------------------

/// Functions in `report.json` with no `fuzzy_match_percent` (exists on only
/// one side).
pub fn find_orphans<'a>(
    functions: &'a [FuncEntry],
    filter: &FuncFilter,
) -> Vec<&'a FuncEntry> {
    filter_functions(
        functions,
        &FuncFilter {
            matched_only: false,
            min_percent: None,
            max_percent: None,
            ..*filter
        },
    )
    .into_iter()
    .filter(|f| f.fuzzy_match_percent.is_none())
    .collect()
}

// ---------------------------------------------------------------------------
// Summary / bucketing
// ---------------------------------------------------------------------------

const DEFAULT_BUCKETS: &[(f64, f64, &str)] = &[
    (0.0, 1.0, "0"),
    (1.0, 50.0, "1-49"),
    (50.0, 80.0, "50-79"),
    (80.0, 90.0, "80-89"),
    (90.0, 95.0, "90-94"),
    (95.0, 99.0, "95-98"),
    (99.0, 100.0, "99-99"),
    (100.0, 100.1, "100"), // 100.1 upper bound catches exactly 100.0
];

pub fn compute_summary(
    functions: &[FuncEntry],
    unit_pattern: Option<&str>,
    by_unit: bool,
) -> Summary {
    let scope: Vec<&FuncEntry> = functions
        .iter()
        .filter(|f| match unit_pattern {
            Some(pat) => f.unit.contains(pat),
            None => true,
        })
        .collect();

    let mut buckets: HashMap<String, BucketStats> = HashMap::new();
    for &(_lo, _hi, label) in DEFAULT_BUCKETS {
        buckets.insert(label.to_string(), BucketStats {
            count: 0,
            code_bytes: 0,
        });
    }

    for f in &scope {
        let pct = f.fuzzy_match_percent.unwrap_or(0.0);
        for &(lo, hi, label) in DEFAULT_BUCKETS {
            if pct >= lo && pct < hi {
                let b = buckets.get_mut(label).unwrap();
                b.count += 1;
                b.code_bytes += f.size;
                break;
            }
        }
    }

    let by_unit = if by_unit {
        let mut unit_map: HashMap<&str, (u64, u64, u64)> = HashMap::new();
        for f in &scope {
            let e = unit_map.entry(&f.unit).or_default();
            e.0 += 1;
            if f.fuzzy_match_percent == Some(100.0) {
                e.1 += 1;
            }
            e.2 += f.size;
        }
        let mut units: Vec<UnitSummary> = unit_map
            .into_iter()
            .map(|(name, (total, matched, code))| UnitSummary {
                name: name.to_string(),
                total_functions: Some(total),
                matched_functions: Some(matched),
                total_code: Some(code.to_string()),
                matched_code: None,
                fuzzy_match_percent: None,
            })
            .collect();
        units.sort_by(|a, b| a.name.cmp(&b.name));
        Some(units)
    } else {
        None
    };

    Summary {
        top_level: TopMeasures::default(),
        buckets,
        by_unit,
    }
}

/// Attach top-level measures to a summary (caller provides them separately
/// since `load_report` only returns the function list).
pub fn attach_top_measures(summary: &mut Summary, measures: TopMeasures) {
    summary.top_level = measures;
}

// ---------------------------------------------------------------------------
// Index cross-reference
// ---------------------------------------------------------------------------

/// Stream `index.jsonl` and build a `mangled -> IndexEntry` map.
/// Only deserialises the fields we need (skips instructions/statements/locals).
/// Also returns the total entry count for reporting.
pub fn load_mangled_index(path: &Path) -> anyhow::Result<HashMap<String, IndexEntry>> {
    let file =
        std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = std::io::BufReader::new(file);
    let mut map = HashMap::new();
    for line in reader.lines() {
        let line = line.context("reading index line")?;
        if line.trim().is_empty() {
            continue;
        }
        let entry: IndexEntry =
            serde_json::from_str(&line).context("deserialising index entry")?;
        map.insert(entry.mangled.clone(), entry);
    }
    Ok(map)
}

/// Join an index map onto a function list by mangled name, setting `enriched`
/// and overwriting `demangled` with the cleaner index name where available.
pub fn enrich(functions: &mut [FuncEntry], index: &HashMap<String, IndexEntry>) {
    for f in functions {
        if let Some(e) = index.get(&f.name) {
            f.demangled = Some(e.name.clone());
            f.enriched = Some(IndexEnrichment {
                rva: e.rva,
                file: e.file.clone(),
            });
        }
    }
}

/// Find index entries whose mangled name does NOT appear in the report
/// function set.
pub fn cross_ref_orphans(
    report_mangled: &HashMap<String, bool>,
    index: &HashMap<String, IndexEntry>,
) -> Vec<IndexEntry> {
    let mut result: Vec<IndexEntry> = index
        .iter()
        .filter(|(mangled, _)| !report_mangled.contains_key(*mangled))
        .map(|(_, entry)| IndexEntry {
            mangled: entry.mangled.clone(),
            name: entry.name.clone(),
            rva: entry.rva,
            size: entry.size,
            file: entry.file.clone(),
        })
        .collect();
    result.sort_by(|a, b| a.mangled.cmp(&b.mangled));
    result
}

/// Attach classification marks to functions by mangled name.
pub fn classify(
    functions: &mut [FuncEntry],
    db: &HashMap<String, crate::orphan_classify::ClassEntry>,
) {
    for f in functions {
        if let Some(e) = db.get(&f.name) {
            f.classification = Some(ClassMark {
                status: e.status.clone(),
                reason: e.reason.clone(),
            });
        }
    }
}

/// Build a set of mangled names from a function list (for cross_ref_orphans).
pub fn mangled_set(functions: &[FuncEntry]) -> HashMap<String, bool> {
    let mut set = HashMap::new();
    for f in functions {
        set.insert(f.name.clone(), true);
    }
    set
}
