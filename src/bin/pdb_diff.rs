use clap::Parser;
use pdb::FallibleIterator;
use std::collections::HashMap;

#[derive(Parser)]
struct Cli {
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    base_pdb: std::path::PathBuf,

    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    target_pdb: std::path::PathBuf,

    /// Windows-style path prefix to strip, e.g. "c:\survarium\sources\"
    #[arg(long)]
    engine_path: String,
}

fn main() -> anyhow::Result<()> {
    let Cli { base_pdb, target_pdb, engine_path } = Cli::parse();

    let mut prefix = engine_path.to_lowercase().replace('/', "\\");
    if !prefix.ends_with('\\') {
        prefix.push('\\');
    }

    let base = collect_checksums(&base_pdb, &prefix)?;
    let target = collect_checksums(&target_pdb, &prefix)?;

    print_diff(&base, &target);
    Ok(())
}

/// Returns a map of lowercased engine-relative .cpp path → raw checksum bytes.
fn collect_checksums(
    path: &std::path::Path,
    engine_prefix: &str,
) -> anyhow::Result<HashMap<String, Vec<u8>>> {
    let file = std::fs::File::open(path)?;
    let mut pdb = pdb::PDB::open(file)?;

    let string_table = pdb.string_table()?;
    let dbi = pdb.debug_information()?;

    let mut result: HashMap<String, Vec<u8>> = HashMap::new();
    let mut modules = dbi.modules()?;

    while let Some(module) = modules.next()? {
        let Some(module_info) = pdb.module_info(&module)? else {
            continue;
        };

        let program = module_info.line_program()?;
        let mut files = program.files();

        while let Some(file_info) = files.next()? {
            let name = file_info.name.to_string_lossy(&string_table)?;
            let name_lower = name.to_lowercase();

            if !name_lower.ends_with(".cpp") {
                continue;
            }
            let Some(relative) = name_lower.strip_prefix(engine_prefix) else {
                continue;
            };

            let key = relative.to_owned();
            if result.contains_key(&key) {
                continue;
            }

            let checksum = match file_info.checksum {
                pdb::FileChecksum::None => vec![],
                pdb::FileChecksum::Md5(b) => b.to_vec(),
                pdb::FileChecksum::Sha1(b) => b.to_vec(),
                pdb::FileChecksum::Sha256(b) => b.to_vec(),
            };

            result.insert(key, checksum);
            break;
        }
    }

    Ok(result)
}

fn print_diff(base: &HashMap<String, Vec<u8>>, target: &HashMap<String, Vec<u8>>) {
    let mut all_keys: Vec<&String> = base.keys().chain(target.keys()).collect();
    all_keys.sort();
    all_keys.dedup();

    let (mut n_match, mut n_diff, mut n_base, mut n_target) = (0usize, 0, 0, 0);

    for key in &all_keys {
        let name = key.replace('\\', "/");
        match (base.get(*key), target.get(*key)) {
            (Some(b), Some(t)) if b == t => { n_match  += 1; println!("MATCH   {name}"); }
            (Some(_), Some(_))           => { n_diff   += 1; println!("DIFF    {name}"); }
            (Some(_), None)              => { n_base   += 1; println!("BASE    {name}"); }
            (None, Some(_))              => { n_target += 1; println!("TARGET  {name}"); }
            (None, None) => unreachable!(),
        }
    }

    println!();
    println!("matched={n_match}  diff={n_diff}  base-only={n_base}  target-only={n_target}");
}
