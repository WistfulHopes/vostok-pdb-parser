//! Recover per-source build flags and the original project (.vcproj) grouping
//! straight from a PDB — and compare two PDBs project-by-project.
//!
//! Each compiland in the DBI stream records:
//!   * `module_name`       — the `.obj` path (under `intermediates\<cfg>\<project>\`),
//!   * `object_file_name`  — the `.lib` it was archived into (or the `.obj`
//!                           itself, for objects passed straight to the linker),
//!   * `S_OBJNAME`         — object path,
//!   * `S_COMPILE3`        — compiler version + coarse flags (LTCG, /GS, language),
//!   * `S_ENVBLOCK`        — key/value strings; for `cl` compilands the `cmd`
//!                           value is the *raw* command line — the real flags,
//!                           including `-MT`/`-MD`, `-O2`, `-Zi`, `-D…`, `-I…`.
//!
//! Grouping modules by `object_file_name` (falling back to the project segment
//! of the intermediates path) reconstructs the libraries/projects.

use clap::Parser;
use pdb::FallibleIterator;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;

#[derive(Parser)]
struct Cli {
    /// PDB to inspect (the "target" side when comparing).
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    pdb: PathBuf,

    /// Compare against this second PDB ("base") and emit a project/config diff.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    compare: Option<PathBuf>,

    /// Write the report to this file instead of stdout.
    #[arg(long, value_hint = clap::ValueHint::FilePath)]
    out: Option<PathBuf>,

    /// Dump raw per-module records (debug) for the first N, or all matching --grep.
    #[arg(long)]
    explore: Option<usize>,

    /// Dump everything for modules whose name/lib contains this substring (debug).
    #[arg(long)]
    grep: Option<String>,

    /// Show full per-project command line(s), not just the CRT/opt summary.
    #[arg(long)]
    full: bool,
}

/// What we recovered about one compiland.
struct Compiland {
    module_name: String,
    object_file_name: String,
    obj_name: String,
    source: Option<String>,
    cmd: Option<String>,
    flags: Option<pdb::CompileFlags>,
    version: String,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    let compilands = read_compilands(&cli.pdb)?;

    // ── debug dumps (single PDB) ─────────────────────────────────────────────
    if cli.explore.is_some() || cli.grep.is_some() {
        dump(&compilands, cli.explore, cli.grep.as_deref());
        return Ok(());
    }

    let mut buf = String::new();

    if let Some(base_path) = &cli.compare {
        let base = read_compilands(base_path)?;
        let target_projects = build_projects(&compilands);
        let base_projects = build_projects(&base);
        write_comparison(
            &mut buf,
            &cli.pdb,
            base_path,
            &target_projects,
            &base_projects,
            cli.full,
        );
    } else {
        let projects = build_projects(&compilands);
        write_report(&mut buf, &projects, cli.full);
    }

    if let Some(out) = &cli.out {
        std::fs::File::create(out)?.write_all(buf.as_bytes())?;
        eprintln!("wrote {}", out.display());
    } else {
        print!("{buf}");
    }
    Ok(())
}

// ── PDB reading ────────────────────────────────────────────────────────────────

fn read_compilands(path: &std::path::Path) -> anyhow::Result<Vec<Compiland>> {
    let file = std::fs::File::open(path)?;
    let mut pdb = pdb::PDB::open(file)?;
    // Compiler-intermediate PDBs sometimes lack the `/names` string table; it's
    // only used for the source-path fallback, so treat it as optional.
    let string_table = pdb.string_table().ok();
    let dbi = pdb.debug_information()?;
    let mut modules = dbi.modules()?;

    let mut out = Vec::new();
    while let Some(module) = modules.next()? {
        let module_name = module.module_name().into_owned();
        let object_file_name = module.object_file_name().into_owned();

        let info = match pdb.module_info(&module)? {
            Some(i) => i,
            None => continue,
        };

        let mut obj_name = String::new();
        let mut version = String::new();
        let mut flags = None;
        let mut cwd = String::new();
        let mut src = String::new();
        let mut cmd = None;

        let mut symbols = info.symbols()?;
        while let Some(sym) = symbols.next()? {
            match sym.parse() {
                Ok(pdb::SymbolData::ObjName(o)) => obj_name = o.name.to_string().into_owned(),
                Ok(pdb::SymbolData::CompileFlags(c)) => {
                    version = c.version_string.to_string().into_owned();
                    flags = Some(c.flags);
                }
                Ok(pdb::SymbolData::EnvBlock(e)) => {
                    let strings: Vec<String> =
                        e.rgsz.iter().map(|s| s.to_string().into_owned()).collect();
                    let mut it = strings.iter();
                    while let (Some(k), Some(v)) = (it.next(), it.next()) {
                        match k.as_str() {
                            "cwd" => cwd = v.clone(),
                            "src" => src = v.clone(),
                            "cmd" => cmd = Some(v.clone()),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }

        let source = if !src.is_empty() {
            Some(join_win(&cwd, &src))
        } else if let Some(st) = &string_table {
            first_source_file(&info, st).unwrap_or(None)
        } else {
            None
        };

        out.push(Compiland {
            module_name,
            object_file_name,
            obj_name,
            source,
            cmd,
            flags,
            version,
        });
    }
    Ok(out)
}

// ── project model ────────────────────────────────────────────────────────────

#[derive(Default)]
struct Project {
    lib: String,
    sources: Vec<String>,
    /// normalized flags -> a representative raw command line.
    flag_sets: BTreeMap<String, String>,
    no_cmd: usize,
    coarse: BTreeSet<String>,
}

impl Project {
    fn source_count(&self) -> usize {
        let mut s: Vec<&String> = self.sources.iter().collect();
        s.sort();
        s.dedup();
        s.len()
    }

    /// CRT model, uniform across the project's flag sets (or "?" if mixed/none).
    fn crt(&self) -> String {
        let crts: BTreeSet<&str> = self.flag_sets.keys().map(|n| crt_of(n)).collect();
        match crts.len() {
            0 => "—".into(),
            1 => crts.into_iter().next().unwrap().into(),
            _ => crts.into_iter().collect::<Vec<_>>().join(" + "),
        }
    }

    /// A compact one-line config: CRT + distinct opt summaries, or the coarse
    /// S_COMPILE3 flags when no command line was recorded.
    fn config(&self) -> String {
        if self.flag_sets.is_empty() {
            let coarse: Vec<String> = self.coarse.iter().cloned().collect();
            return format!(
                "[no cmdline] {}",
                if coarse.is_empty() {
                    "—".into()
                } else {
                    coarse.join(",")
                }
            );
        }
        let summaries: BTreeSet<String> = self.flag_sets.keys().map(|n| summary(n)).collect();
        let mut s: Vec<String> = summaries.into_iter().collect();
        let extra = self.no_cmd;
        let mut out = format!("{}  {}", self.crt(), s.join("  ||  "));
        s.clear();
        if extra > 0 {
            out.push_str(&format!("  (+{extra} files w/o cmdline)"));
        }
        out
    }
}

fn build_projects(compilands: &[Compiland]) -> BTreeMap<String, Project> {
    let mut projects: BTreeMap<String, Project> = BTreeMap::new();
    for c in compilands {
        let key = project_key(c);
        if key.is_empty() {
            continue;
        }
        let p = projects.entry(key).or_default();
        if let Some(s) = &c.source {
            p.sources.push(s.clone());
        } else {
            p.sources.push(c.obj_name.clone());
        }
        if let Some(cmd) = &c.cmd {
            p.flag_sets
                .entry(normalize_cmd(cmd))
                .or_insert_with(|| cmd.clone());
        } else {
            p.no_cmd += 1;
            if let Some(f) = c.flags {
                p.coarse.insert(coarse_summary(f));
            }
        }
        if p.lib.is_empty() || c.object_file_name.to_lowercase().ends_with(".lib") {
            p.lib = c.object_file_name.clone();
        }
    }
    projects
}

// ── single-PDB report ──────────────────────────────────────────────────────────

fn write_report(buf: &mut String, projects: &BTreeMap<String, Project>, full: bool) {
    let _ = writeln!(buf, "# Recovered {} projects from PDB\n", projects.len());
    for (name, p) in projects {
        let mut sources: Vec<&String> = p.sources.iter().collect();
        sources.sort();
        sources.dedup();
        let _ = writeln!(
            buf,
            "════════════════════════════════════════════════════════════"
        );
        let _ = writeln!(buf, "project : {name}");
        let _ = writeln!(buf, "output  : {}", p.lib);
        let _ = writeln!(buf, "sources : {}", sources.len());
        if full && !p.flag_sets.is_empty() {
            for (i, raw) in p.flag_sets.values().enumerate() {
                let _ = writeln!(buf, "cmd[{i}] : {raw}");
            }
        } else {
            let _ = writeln!(buf, "config  : {}", p.config());
        }
        for s in sources {
            let _ = writeln!(buf, "    {s}");
        }
        let _ = writeln!(buf);
    }
}

// ── comparison report (report-2) ────────────────────────────────────────────────

fn write_comparison(
    buf: &mut String,
    target_path: &std::path::Path,
    base_path: &std::path::Path,
    target: &BTreeMap<String, Project>,
    base: &BTreeMap<String, Project>,
    full: bool,
) {
    let names: BTreeSet<&String> = target.keys().chain(base.keys()).collect();

    let both: Vec<&String> = names
        .iter()
        .filter(|n| target.contains_key(**n) && base.contains_key(**n))
        .copied()
        .collect();
    let only_t: Vec<&String> = names
        .iter()
        .filter(|n| target.contains_key(**n) && !base.contains_key(**n))
        .copied()
        .collect();
    let only_b: Vec<&String> = names
        .iter()
        .filter(|n| !target.contains_key(**n) && base.contains_key(**n))
        .copied()
        .collect();

    let _ = writeln!(buf, "# report-2 — project/config comparison\n");
    let _ = writeln!(buf, "TARGET = {}", target_path.display());
    let _ = writeln!(buf, "BASE   = {}", base_path.display());
    let _ = writeln!(
        buf,
        "\nprojects: target={}  base={}  in-both={}  target-only={}  base-only={}\n",
        target.len(),
        base.len(),
        both.len(),
        only_t.len(),
        only_b.len()
    );

    // ── projects present in BOTH, with their configurations ──────────────────
    let _ = writeln!(
        buf,
        "════════════════════════ IN BOTH ════════════════════════\n"
    );
    for n in &both {
        let t = &target[*n];
        let b = &base[*n];
        let t_has = !t.flag_sets.is_empty();
        let b_has = !b.flag_sets.is_empty();
        let status = if t.config() == b.config() {
            "MATCH"
        } else if t_has && b_has && t.crt() != b.crt() {
            "DIFF-CRT"
        } else if !t_has || !b_has {
            // LTCG strips the env-block command line, so one side has no flags to
            // compare — not a real CRT/flag difference.
            "PARTIAL (cmdline one side)"
        } else {
            "DIFF-FLAGS"
        };
        let _ = writeln!(
            buf,
            "── {n}   [{status}]   (target {} src / base {} src)",
            t.source_count(),
            b.source_count()
        );
        let _ = writeln!(buf, "     target: {}", t.config());
        let _ = writeln!(buf, "     base  : {}", b.config());
        if full {
            for (i, raw) in t.flag_sets.values().enumerate() {
                let _ = writeln!(buf, "       target cmd[{i}]: {raw}");
            }
            for (i, raw) in b.flag_sets.values().enumerate() {
                let _ = writeln!(buf, "       base   cmd[{i}]: {raw}");
            }
        }
        let _ = writeln!(buf);
    }

    let _ = writeln!(
        buf,
        "════════════════════ TARGET ONLY ════════════════════════"
    );
    let _ = writeln!(
        buf,
        "(in the original game PDB, missing from the base build)\n"
    );
    for n in &only_t {
        let _ = writeln!(
            buf,
            "── {n}   ({} src)   {}",
            target[*n].source_count(),
            target[*n].config()
        );
    }

    let _ = writeln!(
        buf,
        "\n═════════════════════ BASE ONLY ═════════════════════════"
    );
    let _ = writeln!(
        buf,
        "(in the base build, not present in the original game PDB)\n"
    );
    for n in &only_b {
        let _ = writeln!(
            buf,
            "── {n}   ({} src)   {}",
            base[*n].source_count(),
            base[*n].config()
        );
    }
}

// ── debug dump ──────────────────────────────────────────────────────────────────

fn dump(compilands: &[Compiland], explore: Option<usize>, grep: Option<&str>) {
    let mut shown = 0;
    for c in compilands {
        let hit = match grep {
            Some(g) => {
                let g = g.to_lowercase();
                c.module_name.to_lowercase().contains(&g)
                    || c.object_file_name.to_lowercase().contains(&g)
            }
            None => shown < explore.unwrap_or(0),
        };
        if !hit {
            continue;
        }
        shown += 1;
        println!("════════════════════════════════════════════════════════════");
        println!("module      : {}", c.module_name);
        println!("object_file : {}", c.object_file_name);
        println!("obj         : {}", c.obj_name);
        println!("source      : {}", c.source.as_deref().unwrap_or("<none>"));
        println!("compiler    : {}", c.version);
        if let Some(f) = c.flags {
            println!("compile3    : {f:?}");
        }
        println!("cmd         : {}", c.cmd.as_deref().unwrap_or("<none>"));
    }
}

// ── helpers ──────────────────────────────────────────────────────────────────────

fn project_key(c: &Compiland) -> String {
    let ofn = c.object_file_name.to_lowercase();
    if ofn.ends_with(".lib") {
        return basename(&ofn).trim_end_matches(".lib").to_string();
    }
    intermediates_project(&c.module_name)
        .or_else(|| intermediates_project(&c.obj_name))
        .unwrap_or_default()
}

fn intermediates_project(path: &str) -> Option<String> {
    let lower = path.to_lowercase().replace('/', "\\");
    let idx = lower.find("intermediates\\")?;
    let rest = &lower[idx + "intermediates\\".len()..];
    let mut parts = rest.split('\\');
    let _config = parts.next()?;
    let project = parts.next()?;
    if project.ends_with(".obj") {
        return None;
    }
    Some(project.to_string())
}

fn basename(p: &str) -> &str {
    p.rsplit(['\\', '/']).next().unwrap_or(p)
}

fn join_win(cwd: &str, src: &str) -> String {
    if src.starts_with(".\\") || src.starts_with("./") {
        format!("{}\\{}", cwd.trim_end_matches('\\'), &src[2..])
    } else if src.contains(":\\") || src.starts_with('\\') {
        src.to_string()
    } else {
        format!("{}\\{}", cwd.trim_end_matches('\\'), src)
    }
}

fn normalize_cmd(cmd: &str) -> String {
    let mut toks = tokenize(cmd);
    toks.retain(|t| {
        let l = t.to_lowercase();
        !(l.starts_with("-fo")
            || l.starts_with("/fo")
            || l.starts_with("-fd")
            || l.starts_with("/fd")
            || l.starts_with("-fp")
            || l.starts_with("/fp")
            || l.starts_with("-fr")
            || l.starts_with("/fr"))
    });
    toks.sort();
    toks.join(" ")
}

fn tokenize(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_q = false;
    for ch in s.chars() {
        match ch {
            '"' => {
                in_q = !in_q;
                cur.push(ch);
            }
            c if c.is_whitespace() && !in_q => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn crt_of(norm: &str) -> &'static str {
    let has = |f: &str| norm.split(' ').any(|t| t.eq_ignore_ascii_case(f));
    if has("-MDd") || has("/MDd") {
        "/MDd"
    } else if has("-MD") || has("/MD") {
        "/MD"
    } else if has("-MTd") || has("/MTd") {
        "/MTd"
    } else if has("-MT") || has("/MT") {
        "/MT"
    } else {
        "/MT?"
    }
}

fn summary(norm: &str) -> String {
    let keep: Vec<&str> = norm
        .split(' ')
        .filter(|t| {
            let l = t.to_lowercase();
            l.starts_with("-o")
                || l.starts_with("/o")
                || l.starts_with("-m")
                || l.starts_with("/m")
                || l.starts_with("-g")
                || l.starts_with("/g")
                || l.starts_with("-z")
                || l.starts_with("/z")
                || l.starts_with("-arch")
                || l.starts_with("-fp")
                || l.starts_with("-tc")
                || l.starts_with("-tp")
                || l.starts_with("-eh")
                || l.starts_with("/eh")
                || l.starts_with("-rtc")
        })
        .collect();
    keep.join(" ")
}

fn coarse_summary(f: pdb::CompileFlags) -> String {
    let mut v = Vec::new();
    if f.link_time_codegen {
        v.push("LTCG");
    }
    if f.security_checks {
        v.push("/GS");
    }
    if f.no_debug_info {
        v.push("no-debug");
    }
    if v.is_empty() {
        v.push("(none)");
    }
    v.join(",")
}

fn first_source_file(
    info: &pdb::ModuleInfo,
    string_table: &pdb::StringTable,
) -> pdb::Result<Option<String>> {
    let program = info.line_program()?;
    let mut symbols = info.symbols()?;
    while let Some(sym) = symbols.next()? {
        let offset = match sym.parse() {
            Ok(pdb::SymbolData::Procedure(p)) => p.offset,
            _ => continue,
        };
        let mut lines = program.lines_for_symbol(offset);
        if let Some(line) = lines.next()? {
            let fi = program.get_file_info(line.file_index)?;
            let name = fi.name.to_string_lossy(string_table)?;
            let lower = name.to_lowercase();
            if lower.ends_with(".cpp") || lower.ends_with(".cxx") || lower.ends_with(".c") {
                return Ok(Some(name.into_owned()));
            }
        }
    }
    Ok(None)
}
