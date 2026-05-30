//! `pdb_rich_context` core: emit, per engine function, a block that interleaves
//! the disassembly with the source-level statements that produced it.
//!
//! The two halves come from existing tooling:
//!   * **source half** — the PDB line program gives, per function, a sequence of
//!     `(rva, source-line)` statement boundaries (the same data
//!     `gen_sources.rs` turns into the carcass `FUNCTION BODY` block).
//!   * **disasm half** — the EXE `.text` bytes for the function, decoded with
//!     `iced-x86` (the same slicing `vostok-delinker` does).
//!
//! Both are keyed by the *same* RVA (`proc.offset.to_rva`), so mapping
//! instructions to statements is an exact sorted merge: each statement owns the
//! instructions in `[its_rva, next_statement_rva)`, and its `; 0xNN` annotation
//! is that byte span. Instructions before the first statement (prologue) and any
//! inlined-call bytes fall naturally under the enclosing statement.
//!
//! Base vs target differ only in the statement text: base reads the real source
//! line from disk; target (the original game, no sources) prints the line number
//! as `'<line>'`. Offsets, sizes, labels and disassembly are identical.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;

use pdb::FallibleIterator;
use pdb::PDB;

use iced_x86::Formatter as _;

use object::LittleEndian;
use object::{Object, ObjectSection};

use crate::disasm;
use crate::pdb_parser::PdbParser;

/// RVA -> recovered name, for naming call/data targets in operands. Names prefer
/// the readable module-symbol form (`vostok::foo::bar`); public (mangled) names
/// are kept only as a fallback for library symbols without debug info.
pub struct SymbolMaps {
    pub functions: BTreeMap<usize, String>,
    pub data: BTreeMap<usize, String>,
}

pub struct Options {
    /// Recorded source-path prefix to strip (lowercased, `\`-separated, trailing
    /// `\`), e.g. `c:\survarium\sources\`. Used to identify engine files and to
    /// build relative paths for the output tree.
    pub engine_path: String,
    /// Local directory the engine sources actually live in (base mode). When set,
    /// statement text is read from `source_root / <relative path>`; on a miss we
    /// fall back to the `'<line>'` placeholder.
    pub source_root: Option<PathBuf>,
    /// Target mode = original game, no sources: never attempt source reads.
    pub target_mode: bool,
    /// Output directory for the structure-style tree. `None` => single stream to
    /// stdout (all functions, unfiltered — handy for inspection / target smoke).
    pub out_dir: Option<PathBuf>,
}

pub fn dump_rich_context(pdb_path: &Path, exe_path: &Path, opts: &Options) -> crate::Result<()> {
    // ── EXE: image base + .text bytes ────────────────────────────────────────
    let exe_bytes = std::fs::read(exe_path)?;
    let exe = object::read::pe::PeFile32::parse(exe_bytes.as_slice())?;
    let image_base = exe
        .nt_headers()
        .optional_header
        .image_base
        .get(LittleEndian) as u64;

    let Some(text) = exe.section_by_name(".text") else {
        return crate::error!("EXE has no .text section");
    };
    let text_rva = (text.address() - image_base) as usize;
    let text_data = text.data()?.to_vec();
    drop(exe);
    drop(exe_bytes);

    PdbParser::with(pdb_path, |fmt| {
        let file = std::fs::File::open(pdb_path)?;
        let mut pdb = PDB::open(file)?;

        let address_map = pdb.address_map()?;
        let string_table = pdb.string_table()?;

        let symbols = Rc::new(build_symbol_maps(&mut pdb, &address_map)?);

        // file path -> (function rva -> rendered block)
        let mut by_file: BTreeMap<String, BTreeMap<u32, String>> = BTreeMap::new();
        let mut source_cache: HashMap<String, Option<Vec<String>>> = HashMap::new();

        let dbi = pdb.debug_information()?;
        let mut modules = dbi.modules()?;
        let mut module_id: usize = usize::MAX;

        while let Some(module) = modules.next()? {
            module_id = module_id.wrapping_add(1);

            let Some(module_info) = pdb.module_info(&module)? else {
                continue;
            };
            let program = module_info.line_program()?;
            let mut syms = module_info.symbols()?;

            while let Some(sym) = syms.next()? {
                let proc = match sym.parse() {
                    Ok(pdb::SymbolData::Procedure(proc)) => proc,
                    _ => continue,
                };
                if proc.len == 0 {
                    continue;
                }

                let Some(func_rva) = proc.offset.to_rva(&address_map) else {
                    continue;
                };
                let func_rva = func_rva.0 as usize;
                let size = proc.len as usize;

                // Only functions whose body lives in .text can be disassembled.
                if func_rva < text_rva || func_rva + size > text_rva + text_data.len() {
                    continue;
                }

                // ── statements: (rva, source-line) from the line program ──────
                let mut stmts: Vec<(u32, u32)> = Vec::new();
                let mut file_name: Option<String> = None;
                let mut lines = program.lines_for_symbol(proc.offset);
                while let Some(li) = lines.next()? {
                    if let Some(rva) = li.offset.to_rva(&address_map) {
                        stmts.push((rva.0, li.line_start));
                    }
                    if file_name.is_none() {
                        let fi = program.get_file_info(li.file_index)?;
                        file_name = Some(fi.name.to_string_lossy(&string_table)?.into_owned());
                    }
                }
                if stmts.is_empty() {
                    continue;
                }
                stmts.sort_by_key(|(rva, _)| *rva);
                stmts.dedup_by_key(|(rva, _)| *rva);

                let file_name = file_name.unwrap_or_default();
                let lower = file_name.to_lowercase().replace('/', "\\");
                let rel = lower
                    .strip_prefix(&opts.engine_path)
                    .map(|s| s.to_string());

                // Tree output only carries engine files; stdout carries all.
                if opts.out_dir.is_some() && rel.is_none() {
                    continue;
                }

                let src_lines: Option<&Vec<String>> = match (&opts.source_root, &rel) {
                    (Some(root), Some(rel)) if !opts.target_mode => {
                        let entry = source_cache.entry(rel.clone()).or_insert_with(|| {
                            let path = root.join(rel.replace('\\', "/"));
                            std::fs::read_to_string(&path)
                                .ok()
                                .map(|s| s.lines().map(str::to_string).collect())
                        });
                        entry.as_ref()
                    }
                    _ => None,
                };

                let block = render_function(
                    &fmt,
                    &symbols,
                    image_base,
                    text_rva,
                    &text_data,
                    module_id,
                    &proc,
                    func_rva,
                    size,
                    &stmts,
                    src_lines,
                );

                let key = rel.unwrap_or(file_name);
                by_file.entry(key).or_default().insert(func_rva as u32, block);
            }
        }

        match &opts.out_dir {
            None => write_stdout(&by_file)?,
            Some(dir) => write_tree(dir, by_file)?,
        }

        Ok(())
    })
}

#[allow(clippy::too_many_arguments)]
fn render_function(
    fmt: &PdbParser,
    symbols: &Rc<SymbolMaps>,
    image_base: u64,
    text_rva: usize,
    text_data: &[u8],
    module_id: usize,
    proc: &pdb::ProcedureSymbol,
    func_rva: usize,
    size: usize,
    stmts: &[(u32, u32)],
    src_lines: Option<&Vec<String>>,
) -> String {
    let mut block = String::new();

    let signature = fmt
        .emit_function_orig(&proc.name, module_id, proc.type_index)
        .unwrap_or_else(|_| proc.name.to_string().into_owned());
    let _ = writeln!(block, "{signature}:");

    let off = func_rva - text_rva;
    let code = &text_data[off..off + size];
    let va_base = image_base + func_rva as u64;

    let decoded = disasm::decode(code, va_base);
    let mut formatter = disasm::make_formatter(image_base, decoded.labels.clone(), symbols.clone());

    let func_size = size as u32;
    let func_rva32 = func_rva as u32;

    // Statement starts as offsets within the function. The first start is clamped
    // to 0 so the prologue is grouped under the opening statement.
    let mut starts: Vec<(u32, u32)> = stmts
        .iter()
        .filter(|(rva, _)| *rva >= func_rva32 && *rva - func_rva32 < func_size)
        .map(|(rva, line)| (rva - func_rva32, *line))
        .collect();
    if starts.is_empty() {
        starts.push((0, 0));
    }
    if let Some(first) = starts.first_mut() {
        first.0 = 0;
    }
    starts.sort_by_key(|(off, _)| *off);
    starts.dedup_by_key(|(off, _)| *off);

    for g in 0..starts.len() {
        let (start_off, line) = starts[g];
        let end_off = starts.get(g + 1).map(|(off, _)| *off).unwrap_or(func_size);
        let stmt_size = end_off.saturating_sub(start_off);

        let text = src_lines
            .filter(|_| line != 0)
            .and_then(|lines| lines.get((line as usize).wrapping_sub(1)))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        // Statement header: base shows the real source line (read via the PDB
        // line number); target has no source, so the statement is represented by
        // its byte size alone (the line number is noise for matching).
        match text {
            Some(text) => {
                let _ = writeln!(block, "{text}\t; 0x{stmt_size:x} bytes");
            }
            // No source line — every target statement, and base statements from
            // inlined/headerless code. Anchor by the statement's function-relative
            // offset so it can still be located, in place of the missing source.
            None => {
                let _ = writeln!(block, "[0x{start_off:x}]\t; 0x{stmt_size:x} bytes");
            }
        }

        for insn in &decoded.instructions {
            let ioff = (insn.ip() - va_base) as u32;
            if ioff < start_off || ioff >= end_off {
                continue;
            }
            if let Some(label) = decoded.labels.get(&insn.ip()) {
                let _ = writeln!(block, "{label}:");
            }
            let mut text = String::new();
            formatter.format(insn, &mut text);
            // Per-instruction annotation is the instruction's *size* (hex, with
            // the literal word `bytes`) so it can't be misread as an
            // address/offset. These sum to the statement's `bytes` total above.
            let _ = writeln!(block, "    {text}\t; 0x{:x} bytes", insn.len());
        }

        let _ = writeln!(block);
    }

    block
}

/// Build RVA -> name maps for call/data target annotation. Module symbols
/// (readable names) win; public (mangled) names fill gaps for library code.
fn build_symbol_maps(
    pdb: &mut PDB<'_, std::fs::File>,
    address_map: &pdb::AddressMap,
) -> crate::Result<SymbolMaps> {
    let mut functions: BTreeMap<usize, String> = BTreeMap::new();
    let mut data: BTreeMap<usize, String> = BTreeMap::new();

    {
        let dbi = pdb.debug_information()?;
        let mut modules = dbi.modules()?;
        while let Some(module) = modules.next()? {
            let Some(info) = pdb.module_info(&module)? else {
                continue;
            };
            let mut syms = info.symbols()?;
            while let Some(sym) = syms.next()? {
                match sym.parse() {
                    Ok(pdb::SymbolData::Procedure(p)) => {
                        if let Some(rva) = p.offset.to_rva(address_map) {
                            functions
                                .entry(rva.0 as usize)
                                .or_insert_with(|| p.name.to_string().into_owned());
                        }
                    }
                    Ok(pdb::SymbolData::Thunk(t)) => {
                        if let Some(rva) = t.offset.to_rva(address_map) {
                            functions
                                .entry(rva.0 as usize)
                                .or_insert_with(|| t.name.to_string().into_owned());
                        }
                    }
                    Ok(pdb::SymbolData::Data(d)) => {
                        if let Some(rva) = d.offset.to_rva(address_map) {
                            data.entry(rva.0 as usize)
                                .or_insert_with(|| d.name.to_string().into_owned());
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    let global = pdb.global_symbols()?;
    let mut it = global.iter();
    while let Some(sym) = it.next()? {
        if let Ok(pdb::SymbolData::Public(p)) = sym.parse() {
            if let Some(rva) = p.offset.to_rva(address_map) {
                let rva = rva.0 as usize;
                if p.function {
                    functions
                        .entry(rva)
                        .or_insert_with(|| p.name.to_string().into_owned());
                } else {
                    data.entry(rva)
                        .or_insert_with(|| p.name.to_string().into_owned());
                }
            }
        }
    }

    Ok(SymbolMaps { functions, data })
}

fn write_stdout(by_file: &BTreeMap<String, BTreeMap<u32, String>>) -> crate::Result<()> {
    let stdout = std::io::stdout();
    let mut w = stdout.lock();
    for (file, funs) in by_file {
        writeln!(w, "// ===== {file} =====\n")?;
        for block in funs.values() {
            write!(w, "{block}")?;
        }
    }
    Ok(())
}

/// Write the structure-style tree: `<dir>/sources/<relative path>`, one file per
/// source file, functions in RVA order. `by_file` keys are already unique
/// relative paths, so no collision handling is needed.
fn write_tree(
    dir: &Path,
    by_file: BTreeMap<String, BTreeMap<u32, String>>,
) -> crate::Result<()> {
    let root = dir.join("sources");

    for (rel, funs) in by_file {
        let relative = rel.replace('\\', "/");
        let full = root.join(relative.trim_start_matches('/'));

        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut file = std::fs::File::create(&full)?;
        for block in funs.values() {
            write!(file, "{block}")?;
        }
    }

    Ok(())
}
