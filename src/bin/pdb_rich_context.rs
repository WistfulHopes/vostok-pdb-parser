//! Emit rich per-function context (disassembly interleaved with source-level
//! statements) for binary matching.
//!
//! Base (compiled) example:
//!   pdb_rich_context \
//!     --pdb vostok/binaries/Win32/survarium-dx11-win32-gold.pdb \
//!     --exe vostok/binaries/Win32/survarium-dx11-win32-gold.exe \
//!     --engine-path 'c:\survarium\sources' \
//!     --source-root vostok/sources \
//!     --mode base --out out/rich/base
//!
//! Target (original game, no sources) to stdout for inspection:
//!   pdb_rich_context --pdb survarium.pdb --exe survarium.exe --mode target

use std::path::PathBuf;

use clap::Parser;
use clap::ValueEnum;

use vostok_pdb_parser::rich_context::{dump_rich_context, Options};

#[derive(Copy, Clone, ValueEnum)]
enum Mode {
    /// Compiled build: read real source lines from `--source-root`.
    Base,
    /// Original game: no sources, statements show line-number placeholders.
    Target,
}

#[derive(Parser)]
struct Cli {
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    pdb: PathBuf,

    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    exe: PathBuf,

    /// Recorded source-path prefix to strip (identifies engine files).
    #[arg(long, default_value = r"c:\survarium\sources")]
    engine_path: String,

    /// Local engine source root to read statement text from (base mode).
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    source_root: Option<PathBuf>,

    #[arg(long, value_enum, default_value_t = Mode::Target)]
    mode: Mode,

    /// Output directory (structure-style tree). Omit to print to stdout.
    #[arg(long, value_hint = clap::ValueHint::DirPath)]
    out: Option<PathBuf>,
}

fn main() {
    let cli = Cli::parse();

    let mut engine_path = cli.engine_path.to_lowercase().replace('/', "\\");
    if !engine_path.ends_with('\\') {
        engine_path.push('\\');
    }

    let opts = Options {
        engine_path,
        source_root: cli.source_root,
        target_mode: matches!(cli.mode, Mode::Target),
        out_dir: cli.out,
    };

    if let Err(error) = dump_rich_context(&cli.pdb, &cli.exe, &opts) {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
