//! Query the function index built by `pdb_rich_context --out`.
//!
//! Build once (a complete rebuild), then query repeatedly — each query reads
//! only `index.jsonl`, never the PDB:
//!
//!   pdb_rich_context --pdb survarium.pdb --exe survarium.exe \
//!     --mode target --out out/rich
//!   pdb_rich_query --index out/rich/index.jsonl --function contact_test
//!   pdb_rich_query --index out/rich/index.jsonl --rva 0x1a2b3c
//!   pdb_rich_query --index out/rich/index.jsonl --va 0x41a2b3c
//!   pdb_rich_query --index out/rich/index.jsonl --function bt_ghost_object --list

use std::path::PathBuf;

use clap::Parser;

use vostok_pdb_parser::rich_query::{Query, search};
use vostok_pdb_parser::rich_render::render_listing;

#[derive(Parser)]
struct Cli {
    /// Path to the `index.jsonl` produced by `pdb_rich_context --out`.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    index: PathBuf,

    /// Case-insensitive substring of the function signature to return.
    #[arg(long)]
    function: Option<String>,

    /// Exact function RVA (hex, e.g. 0x1a2b3c).
    #[arg(long, value_parser = parse_hex)]
    rva: Option<u32>,

    /// Absolute VA (hex) selecting the function that contains it - the twin of
    /// --rva for the addresses listings/carcasses print (va = image_base + rva).
    #[arg(long, value_parser = parse_hex)]
    va: Option<u32>,

    /// List matches as `rva  file  signature` instead of printing their bodies.
    #[arg(long)]
    list: bool,
}

fn parse_hex(s: &str) -> Result<u32, std::num::ParseIntError> {
    let s = s
        .strip_prefix("0x")
        .or_else(|| s.strip_prefix("0X"))
        .unwrap_or(s);
    u32::from_str_radix(s, 16)
}

fn main() {
    let cli = Cli::parse();

    let query = Query {
        name: cli.function.as_deref(),
        rva: cli.rva,
        containing_va: cli.va,
        ..Default::default()
    };

    let hits = match search(&cli.index, &query) {
        Ok(hits) => hits,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    };

    if hits.is_empty() {
        eprintln!("no function matched");
        std::process::exit(2);
    }

    for e in &hits {
        if cli.list {
            println!("0x{:06x}  {}\t{}", e.rva, e.file, e.name);
        } else {
            print!("{}", render_listing(e));
        }
    }
}
