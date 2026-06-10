//! Fetch matching context for one function from pre-built indexes.
//!
//! "Rebuild completely, then query on top": point `pdb_fetch` at a target index
//! and/or a base index, select a function by name/rva, and ask for one or more
//! views. Base and target join by signature (`name`), which is identical across
//! the two PDBs.
//!
//!   # the target listing (what to match)
//!   pdb_fetch --target-index out/target/index.jsonl \
//!     --function contact_test --view target
//!
//!   # base vs target diff (computed on raw asm, before metadata)
//!   pdb_fetch --target-index out/target/index.jsonl \
//!     --base-index out/base/index.jsonl \
//!     --function contact_test --view diff
//!
//!   # just the statement structure (sizes + lengths)
//!   pdb_fetch --target-index out/target/index.jsonl \
//!     --rva 0x573750 --view structure
//!
//!   # base vs target *structure* diff (aligned statement skeletons)
//!   pdb_fetch --target-index out/target/index.jsonl \
//!     --base-index out/base/index.jsonl \
//!     --function contact_test --view structure-diff
//!
//! `--view` takes a comma list: target,base,structure,diff,structure-diff
//! (default chosen from which indexes are supplied).

use std::path::Path;
use std::path::PathBuf;

use clap::Parser;

use vostok_pdb_parser::rich_callees;
use vostok_pdb_parser::rich_context::FunctionEntry;
use vostok_pdb_parser::rich_diff;
use vostok_pdb_parser::rich_objdiff;
use vostok_pdb_parser::rich_query::{search, Query};
use vostok_pdb_parser::rich_render::{
    render_info, render_listing, render_listing_statement, render_structure,
};
use vostok_pdb_parser::rich_structure_diff::render_structure_diff;

#[derive(Parser)]
#[command(group(clap::ArgGroup::new("statement").args(["index", "offset", "address"]).multiple(false)))]
struct Cli {
    /// Target index.jsonl (the original game; what we match against).
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    target_index: Option<PathBuf>,

    /// Base index.jsonl (our compiled build).
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    base_index: Option<PathBuf>,

    /// Case-insensitive substring of the function signature to fetch.
    #[arg(long)]
    function: Option<String>,

    /// Exact function RVA (hex, e.g. 0x573750).
    #[arg(long, value_parser = parse_hex)]
    rva: Option<u32>,

    /// Absolute VA (hex) selecting the function that contains it - the twin of
    /// --rva for the addresses listings/carcasses print (va = image_base + rva),
    /// so no manual image-base subtraction.
    #[arg(long, value_parser = parse_hex)]
    va: Option<u32>,

    // Select ONE body statement for `--view target`/`base` (mutually exclusive) -
    // shows just that statement's disassembly, for comparing one diverging
    // statement target-vs-base without pulling the whole function into context.
    /// 1-based body-statement index, matching the `--view structure` rows.
    #[arg(long)]
    index: Option<usize>,

    /// Function-relative offset (hex) - the `offst` column; the statement whose
    /// byte range contains it is shown.
    #[arg(long, value_parser = parse_hex)]
    offset: Option<u32>,

    /// Absolute VA (hex) - the `address` column; the statement whose byte range
    /// contains it is shown.
    #[arg(long, value_parser = parse_hex)]
    address: Option<u32>,

    /// Comma-separated views: target, base, structure, diff, structure-diff.
    /// Default depends on which indexes are supplied (diff if both, else the
    /// available side).
    #[arg(long, value_delimiter = ',')]
    view: Vec<String>,

    /// Delinker base `.obj` dir (e.g. binaries/objdiff/base). With its target
    /// counterpart, `--view diff` uses the operand-aware objdiff-core backend.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    objdiff_base_dir: Option<PathBuf>,

    /// Delinker target `.obj` dir (e.g. binaries/objdiff/target).
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    objdiff_target_dir: Option<PathBuf>,
}

fn parse_hex(s: &str) -> Result<u32, std::num::ParseIntError> {
    let s = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    u32::from_str_radix(s, 16)
}

/// Resolve the `--index`/`--offset`/`--address` selector against `f` to a 1-based
/// body-statement index. Returns `None` when no selector was given (render the whole
/// function); `Some(0)` when an offset/address matched no statement (render then
/// reports it as out of range). The flags are mutually exclusive (clap enforces it).
fn resolve_statement(f: &FunctionEntry, cli: &Cli) -> Option<usize> {
    if let Some(n) = cli.index {
        return Some(n);
    }
    let func_va = f.image_base.wrapping_add(f.rva);
    let off = cli.offset.or_else(|| cli.address.map(|a| a.wrapping_sub(func_va)))?;
    Some(
        (1..f.statements.len().saturating_sub(1))
            .find(|&i| off >= f.statements[i].off && off < f.statements[i].off + f.statements[i].size)
            .unwrap_or(0),
    )
}

/// First entry matching the selector in `index`, if any.
fn first_match(index: &Path, query: &Query) -> Result<Option<FunctionEntry>, String> {
    let mut hits = search(index, query).map_err(|e| e.to_string())?;
    if hits.len() > 1 {
        eprintln!(
            "note: {} matches in {}, using the first ({}); narrow with --rva/--va for an exact pick",
            hits.len(),
            index.display(),
            hits[0].name,
        );
    }
    Ok(hits.drain(..).next())
}

/// Resolve the entry in `index` whose identity equals `target`'s, preferring the
/// `mangled` symbol (identical across the two PDBs per FunctionEntry docs) and
/// falling back to an exact demangled-`name` match. The target rva is NOT used —
/// the base rva differs for the same function. Returns the (sole) match, `None`
/// when nothing matches, or `Err(_)` when the name is ambiguous (an overload set
/// the mangled symbol failed to disambiguate).
fn resolve_by_identity(
    index: &Path,
    target: &FunctionEntry,
) -> Result<Option<FunctionEntry>, String> {
    // The shared mangled symbol is the precise key; match it first. (The demangled
    // name can differ across the two PDBs — e.g. `const` on by-value params — so we
    // cannot pre-filter by name here.)
    let mut by_mangled = search(
        index,
        &Query {
            mangled: Some(&target.mangled),
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())?;
    if !by_mangled.is_empty() {
        return Ok(by_mangled.drain(..).next());
    }

    // Fall back to an exact demangled-name match (handles entries whose mangled
    // form differs but signature is identical).
    let mut by_name: Vec<FunctionEntry> = search(
        index,
        &Query {
            name: Some(&target.name),
            ..Default::default()
        },
    )
    .map_err(|e| e.to_string())?
    .into_iter()
    .filter(|e| e.name == target.name)
    .collect();
    match by_name.len() {
        0 => Ok(None),
        1 => Ok(by_name.drain(..).next()),
        n => Err(format!(
            "structure-diff: function '{}' is ambiguous in the base index ({n} entries, \
             mangled '{}' did not disambiguate); pass --rva for the target side",
            target.name, target.mangled
        )),
    }
}

fn main() {
    let cli = Cli::parse();
    let _log = LogGuard::start(resolve_log_path(&cli));

    if cli.target_index.is_none() && cli.base_index.is_none() {
        eprintln!("error: supply --target-index and/or --base-index");
        std::process::exit(2);
    }
    if cli.function.is_none() && cli.rva.is_none() && cli.va.is_none() && cli.address.is_none() {
        eprintln!("error: select a function with --function, --rva, --va, or --address");
        std::process::exit(2);
    }

    // --va selects by absolute address outright; an absolute --address also
    // SELECTS the function it falls in when no other selector is given (its
    // primary role is picking a statement). --offset is function-relative, so
    // it can't select on its own.
    let containing = cli.va.or(if cli.function.is_none() && cli.rva.is_none() {
        cli.address
    } else {
        None
    });
    let query = Query {
        name: cli.function.as_deref(),
        rva: cli.rva,
        containing_va: containing,
        ..Default::default()
    };

    // Resolve the TARGET side first, by name (narrowed by --rva), exactly the way
    // the diff/structure views select their entry.
    let target = match cli.target_index.as_deref().map(|p| first_match(p, &query)) {
        Some(Ok(t)) => t,
        Some(Err(e)) => fail(&e),
        None => None,
    };
    // Resolve the BASE side INDEPENDENTLY. When we have a target, join by the
    // target's identity (mangled symbol, then exact name) — never by the target's
    // rva, which differs across the two PDBs. With no target (base-only run), fall
    // back to the raw selector.
    let base = match cli.base_index.as_deref() {
        None => None,
        Some(p) => match &target {
            Some(t) => match resolve_by_identity(p, t) {
                Ok(b) => b,
                Err(e) => fail(&e),
            },
            None => {
                let q = Query {
                    name: cli.function.as_deref(),
                    rva: cli.rva,
                    containing_va: containing,
                    ..Default::default()
                };
                match first_match(p, &q) {
                    Ok(b) => b,
                    Err(e) => fail(&e),
                }
            }
        },
    };

    if target.is_none() && base.is_none() {
        eprintln!("no function matched");
        std::process::exit(1);
    }

    // Default views from what is available.
    let views = if !cli.view.is_empty() {
        cli.view.clone()
    } else if target.is_some() && base.is_some() {
        vec!["diff".into()]
    } else if target.is_some() {
        vec!["target".into()]
    } else {
        vec!["base".into()]
    };

    for view in &views {
        match view.as_str() {
            "target" => match &target {
                Some(t) => match resolve_statement(t, &cli) {
                    Some(n) => print!("{}", render_listing_statement(t, n)),
                    None => print!("{}", render_listing(t)),
                },
                None => eprintln!("(no target match for --view target)"),
            },
            "base" => match &base {
                Some(b) => match resolve_statement(b, &cli) {
                    Some(n) => print!("{}", render_listing_statement(b, n)),
                    None => print!("{}", render_listing(b)),
                },
                None => eprintln!("(no base match for --view base)"),
            },
            "structure" => match target.as_ref().or(base.as_ref()) {
                Some(f) => print!("{}", render_structure(f)),
                None => {}
            },
            "info" => match target.as_ref().or(base.as_ref()) {
                Some(f) => print!("{}", render_info(f)),
                None => {}
            },
            "callees" => {
                // Resolve against the side the function came from (prefer target).
                let (entry, index) = match &target {
                    Some(t) => (Some(t), cli.target_index.as_deref()),
                    None => (base.as_ref(), cli.base_index.as_deref()),
                };
                if let Some(f) = entry {
                    let callees = rich_callees::extract(f);
                    let resolved = match index {
                        Some(p) => rich_callees::resolve(p, &callees).unwrap_or_else(|e| {
                            eprintln!("(callee resolve failed: {e})");
                            Default::default()
                        }),
                        None => Default::default(),
                    };
                    print!("{}", rich_callees::render(f, &callees, &resolved));
                }
            }
            "diff" => match (&base, &target) {
                (Some(b), Some(t)) => print_diff(&cli, b, t),
                _ => eprintln!("(--view diff needs both --base-index and --target-index)"),
            },
            "structure-diff" => match (&base, &target) {
                (Some(b), Some(t)) => {
                    print!("{}", render_structure_diff(b, t))
                }
                _ => {
                    // Distinguish a genuinely absent index flag (keep the original
                    // message) from a side that failed to RESOLVE though its flag
                    // was passed (precise, names which side and why).
                    if cli.target_index.is_none() || cli.base_index.is_none() {
                        eprintln!(
                            "(--view structure-diff needs both --base-index and --target-index)"
                        );
                    } else if target.is_none() {
                        eprintln!(
                            "structure-diff: function {} not found in TARGET index \
                             (overload? wrong name? pass --rva to pin it)",
                            describe_selector(&cli)
                        );
                    } else {
                        eprintln!(
                            "structure-diff: function '{}' not found in BASE index \
                             (is it built? overload? pass --rva for the target side)",
                            target.as_ref().map(|t| t.name.as_str()).unwrap_or("?")
                        );
                    }
                }
            },
            other => {
                eprintln!("(unknown view '{other}'; use target|base|structure|diff|structure-diff)")
            }
        }
    }
}

/// Prefer the operand-aware objdiff-core backend when the delinker `.obj` dirs
/// are given; otherwise (or on any miss) fall back to the built-in text diff.
fn print_diff(cli: &Cli, base: &FunctionEntry, target: &FunctionEntry) {
    if let (Some(bdir), Some(tdir)) = (&cli.objdiff_base_dir, &cli.objdiff_target_dir) {
        let bobj = bdir.join(format!("{}.obj", base.file));
        let tobj = tdir.join(format!("{}.obj", target.file));
        match rich_objdiff::diff(&bobj, &tobj, &target.mangled) {
            Ok(Some(result)) => {
                print!("{}", rich_objdiff::render(&result, base));
                return;
            }
            Ok(None) => eprintln!(
                "(objdiff: '{}' not found in {} / {}; falling back to text diff)",
                target.mangled,
                bobj.display(),
                tobj.display()
            ),
            Err(e) => eprintln!("(objdiff failed: {e}; falling back to text diff)"),
        }
    }

    let d = rich_diff::diff(base, target);
    print!("{}", rich_diff::render_unified(base, target, &d));
}

/// Human-readable rendering of the function selector, for error messages.
fn describe_selector(cli: &Cli) -> String {
    let addr = cli
        .rva
        .map(|v| format!("rva 0x{v:x}"))
        .or_else(|| cli.va.map(|v| format!("va 0x{v:x}")));
    match (&cli.function, addr) {
        (Some(n), Some(a)) => format!("'{n}' ({a})"),
        (Some(n), None) => format!("'{n}'"),
        (None, Some(a)) => a,
        (None, None) => "<none>".to_string(),
    }
}

/// Audit log: append one tab-separated line per invocation -
/// `<timestamp>  <git-branch>  <all flags>` - so the agent's tool usage (which
/// view, which function/address, from which worktree, when) is reviewable. These
/// reads are fast, so execution time is not recorded. Logs on Drop, so it fires
/// for any normal completion.
struct LogGuard {
    when: chrono::DateTime<chrono::Local>,
    path: Option<PathBuf>,
}

impl LogGuard {
    fn start(path: Option<PathBuf>) -> Self {
        Self {
            when: chrono::Local::now(),
            path,
        }
    }
}

impl Drop for LogGuard {
    fn drop(&mut self) {
        let Some(path) = &self.path else {
            return;
        };
        use chrono::Timelike as _;
        let args: Vec<String> = std::env::args().skip(1).collect();
        let line = format!(
            "[{}.{:02}][{}]: {}\n",
            self.when.format("%Y-%m-%d %H:%M:%S"),
            self.when.nanosecond() / 10_000_000,
            current_branch(),
            args.join(" "),
        );
        use std::io::Write as _;
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = f.write_all(line.as_bytes());
        }
    }
}

/// Where to write the audit log: `$PDB_FETCH_LOG` if set (an override), else
/// `<binaries>/<tool>.log` - `<binaries>` derived from the index path
/// (`binaries/rich/<side>/index.jsonl` -> `binaries`), `<tool>` from argv[0].
/// `None` (no logging) when neither is available.
fn resolve_log_path(cli: &Cli) -> Option<PathBuf> {
    if let Ok(p) = std::env::var("PDB_FETCH_LOG") {
        return Some(PathBuf::from(p));
    }
    let idx = cli.target_index.as_deref().or(cli.base_index.as_deref())?;
    let binaries = idx.ancestors().nth(3)?;
    let tool = std::env::args()
        .next()
        .and_then(|a| Path::new(&a).file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "pdb_fetch".into());
    Some(binaries.join(format!("{tool}.log")))
}

/// Current git branch of the working dir (for the audit log); `?` if unavailable.
fn current_branch() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "?".into())
}

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}
