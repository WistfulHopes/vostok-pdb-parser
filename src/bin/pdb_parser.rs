use clap::Parser;
use vostok_pdb_parser::{Cli, GenFlags};

fn main() {
    let Cli {
        pdb_path,
        output_path,
        engine_path,
        as_base,
        no_cache,
        skip_non_engine_headers,
    } = Cli::parse();

    let flags = {
        let mut flags = GenFlags::empty();
        flags.set(GenFlags::AS_BASE, as_base);
        flags.set(GenFlags::NO_CACHE, no_cache);
        flags.set(GenFlags::SKIP_NON_ENGINE_HEADERS, skip_non_engine_headers);
        flags
    };

    let mut engine_path = engine_path.to_lowercase().replace('/', "\\");
    if !engine_path.ends_with('\\') {
        engine_path.push('\\');
    }

    if let Err(error) =
        vostok_pdb_parser::dump_pdb::dump_pdb(&pdb_path, &output_path, &engine_path, flags)
    {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
