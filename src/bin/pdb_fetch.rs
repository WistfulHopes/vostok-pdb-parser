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
//! `--view` takes a comma list: target,base,structure,diff (default chosen from
//! which indexes are supplied).

use std::path::Path;
use std::path::PathBuf;

use clap::Parser;

use vostok_pdb_parser::rich_callees;
use vostok_pdb_parser::rich_context::FunctionEntry;
use vostok_pdb_parser::rich_diff;
use vostok_pdb_parser::rich_objdiff;
use vostok_pdb_parser::rich_query::{search, Query};
use vostok_pdb_parser::rich_render::{render_info, render_listing, render_structure};

#[derive(Parser)]
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

    /// Comma-separated views: target, base, structure, diff. Default depends on
    /// which indexes are supplied (diff if both, else the available side).
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

/// First entry matching the selector in `index`, if any.
fn first_match(index: &Path, query: &Query) -> Result<Option<FunctionEntry>, String> {
    let mut hits = search(index, query).map_err(|e| e.to_string())?;
    if hits.len() > 1 {
        eprintln!(
            "note: {} matches in {}, using the first ({}); narrow with --rva for an exact pick",
            hits.len(),
            index.display(),
            hits[0].name,
        );
    }
    Ok(hits.drain(..).next())
}

fn main() {
    let cli = Cli::parse();

    if cli.target_index.is_none() && cli.base_index.is_none() {
        eprintln!("error: supply --target-index and/or --base-index");
        std::process::exit(2);
    }
    if cli.function.is_none() && cli.rva.is_none() {
        eprintln!("error: select a function with --function or --rva");
        std::process::exit(2);
    }

    let query = Query {
        name: cli.function.as_deref(),
        rva: cli.rva,
    };

    let target = match cli.target_index.as_deref().map(|p| first_match(p, &query)) {
        Some(Ok(t)) => t,
        Some(Err(e)) => fail(&e),
        None => None,
    };
    // Join base to the resolved target by exact name when we have one, so the two
    // sides are the same function even if the selector was a loose substring.
    let base = match cli.base_index.as_deref() {
        None => None,
        Some(p) => {
            let q = match &target {
                Some(t) => Query { name: Some(&t.name), rva: None },
                None => Query { name: cli.function.as_deref(), rva: cli.rva },
            };
            match first_match(p, &q) {
                Ok(b) => b,
                Err(e) => fail(&e),
            }
        }
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
                Some(t) => print!("{}", render_listing(t)),
                None => eprintln!("(no target match for --view target)"),
            },
            "base" => match &base {
                Some(b) => print!("{}", render_listing(b)),
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
            other => eprintln!("(unknown view '{other}'; use target|base|structure|diff)"),
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

fn fail(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}
