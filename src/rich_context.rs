//! `pdb_rich_context` core: build, per engine function, a *structured* record
//! that pairs the disassembly with the source-level statements that produced it.
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
//! instructions in `[its_rva, next_statement_rva)`, and its size is that span.
//!
//! We deliberately store this as **structured data** ([`FunctionEntry`]), not a
//! pre-rendered string: the query/fetch tools render it into whichever view is
//! asked for (full listing, structure-only, or a base↔target diff computed on
//! the raw instruction text *before* any metadata is attached). Rendering lives
//! in [`crate::rich_render`]; diffing in [`crate::rich_diff`].
//!
//! Base vs target differ only in [`Statement::source`]: base reads the real
//! source line from disk; target (the original game, no sources) leaves it
//! `None`. Offsets, sizes, labels and disassembly are identical.

use std::collections::BTreeMap;
use std::collections::HashMap;
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
use crate::rich_render;

/// RVA -> recovered name, for naming call/data targets in operands. Names prefer
/// the readable module-symbol form (`vostok::foo::bar`); public (mangled) names
/// are kept only as a fallback for library symbols without debug info.
pub struct SymbolMaps {
    pub functions: BTreeMap<usize, String>,
    pub data: BTreeMap<usize, String>,
    /// RVA -> decorated (mangled) name, from Public symbols only. Module
    /// Procedure symbols carry only the undecorated `ns::func`; the COFF objects
    /// (and thus objdiff) key on the decorated name, which lives here.
    pub public_functions: BTreeMap<usize, String>,
}

/// One decoded instruction. `text` is the rendered mnemonic+operands with branch
/// targets resolved to local labels and call/data targets to symbol names — this
/// is the *normalized* form the diff aligns on.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Instruction {
    /// Offset from the function start.
    pub off: u32,
    /// Instruction length in bytes.
    pub len: u8,
    /// Rendered mnemonic + operands.
    pub text: String,
    /// Local label sitting at this offset (e.g. `.1`), if it is a branch target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// One source-level statement (a line-program boundary) and the byte span it
/// compiled to.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Statement {
    /// Offset from the function start where this statement's code begins.
    pub off: u32,
    /// Byte size of this statement's instruction run (span to the next statement).
    pub size: u32,
    /// Source line number (0 = unknown).
    pub line: u32,
    /// Real source text (base only; `None` for target / inlined-headerless code).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// One function's full structured record, serialized one-per-line into
/// `<out>/index.jsonl`. Base and target functions join by [`FunctionEntry::name`]
/// (the signature is identical across the two PDBs).
#[derive(serde::Serialize, serde::Deserialize)]
pub struct FunctionEntry {
    /// Full demangled signature.
    pub name: String,
    /// Decorated (mangled) COFF symbol name — the join key to the delinker `.obj`
    /// files for the objdiff backend.
    pub mangled: String,
    /// Function RVA (image-relative) — the merge key shared with the line program.
    pub rva: u32,
    /// Function length in bytes.
    pub size: u32,
    /// Source file, `/`-separated (engine-relative in tree mode).
    pub file: String,
    /// Statement boundaries, sorted by offset.
    pub statements: Vec<Statement>,
    /// Instructions, in address order.
    pub instructions: Vec<Instruction>,
}

pub struct Options {
    /// Recorded source-path prefix to strip (lowercased, `\`-separated, trailing
    /// `\`), e.g. `c:\survarium\sources\`. Used to identify engine files and to
    /// build relative paths for the output tree.
    pub engine_path: String,
    /// Local directory the engine sources actually live in (base mode). When set,
    /// statement text is read from `source_root / <relative path>`; on a miss the
    /// statement keeps `source: None`.
    pub source_root: Option<PathBuf>,
    /// Target mode = original game, no sources: never attempt source reads.
    pub target_mode: bool,
    /// Output directory for the structure-style tree + index. `None` => render a
    /// single stream to stdout (all functions, unfiltered — handy for inspection).
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

        let mut entries: Vec<FunctionEntry> = Vec::new();
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
                let rel = lower.strip_prefix(&opts.engine_path).map(|s| s.to_string());

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

                let signature = fmt
                    .emit_function_orig(&proc.name, module_id, proc.type_index)
                    .unwrap_or_else(|_| proc.name.to_string().into_owned());

                let file = rel.unwrap_or(file_name).replace('\\', "/");
                // Decorated name for the objdiff/.obj join; module symbols only
                // give the undecorated form, so prefer the Public-symbol name.
                let mangled = symbols
                    .public_functions
                    .get(&func_rva)
                    .cloned()
                    .unwrap_or_else(|| proc.name.to_string().into_owned());

                entries.push(build_function(
                    signature, mangled, &symbols, image_base, text_rva, &text_data, func_rva, size,
                    &stmts, src_lines, file,
                ));
            }
        }

        match &opts.out_dir {
            None => write_stdout(&entries)?,
            Some(dir) => {
                write_tree(dir, &entries)?;
                write_index(dir, &entries)?;
            }
        }

        Ok(())
    })
}

/// Decode a function's bytes + merge in its statement boundaries, producing the
/// structured [`FunctionEntry`]. No rendering happens here.
#[allow(clippy::too_many_arguments)]
fn build_function(
    signature: String,
    mangled: String,
    symbols: &Rc<SymbolMaps>,
    image_base: u64,
    text_rva: usize,
    text_data: &[u8],
    func_rva: usize,
    size: usize,
    stmts: &[(u32, u32)],
    src_lines: Option<&Vec<String>>,
    file: String,
) -> FunctionEntry {
    let off = func_rva - text_rva;
    let code = &text_data[off..off + size];
    let va_base = image_base + func_rva as u64;

    let decoded = disasm::decode(code, va_base);
    let mut formatter = disasm::make_formatter(image_base, decoded.labels.clone(), symbols.clone());

    let instructions = decoded
        .instructions
        .iter()
        .map(|insn| {
            let mut text = String::new();
            formatter.format(insn, &mut text);
            Instruction {
                off: (insn.ip() - va_base) as u32,
                len: insn.len() as u8,
                text,
                label: decoded.labels.get(&insn.ip()).cloned(),
            }
        })
        .collect();

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

    let statements = (0..starts.len())
        .map(|g| {
            let (start_off, line) = starts[g];
            let end_off = starts.get(g + 1).map(|(off, _)| *off).unwrap_or(func_size);
            let source = src_lines
                .filter(|_| line != 0)
                .and_then(|lines| lines.get((line as usize).wrapping_sub(1)))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            Statement {
                off: start_off,
                size: end_off.saturating_sub(start_off),
                line,
                source,
            }
        })
        .collect();

    FunctionEntry {
        name: signature,
        mangled,
        rva: func_rva as u32,
        size: size as u32,
        file,
        statements,
        instructions,
    }
}

/// Build RVA -> name maps for call/data target annotation. Module symbols
/// (readable names) win; public (mangled) names fill gaps for library code.
fn build_symbol_maps(
    pdb: &mut PDB<'_, std::fs::File>,
    address_map: &pdb::AddressMap,
) -> crate::Result<SymbolMaps> {
    let mut functions: BTreeMap<usize, String> = BTreeMap::new();
    let mut data: BTreeMap<usize, String> = BTreeMap::new();
    let mut public_functions: BTreeMap<usize, String> = BTreeMap::new();

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
                    let name = p.name.to_string().into_owned();
                    public_functions.entry(rva).or_insert_with(|| name.clone());
                    functions.entry(rva).or_insert(name);
                } else {
                    data.entry(rva)
                        .or_insert_with(|| p.name.to_string().into_owned());
                }
            }
        }
    }

    Ok(SymbolMaps { functions, data, public_functions })
}

/// Group entries by file, each file's functions sorted by RVA — borrowed.
fn group_by_file(entries: &[FunctionEntry]) -> BTreeMap<&str, BTreeMap<u32, &FunctionEntry>> {
    let mut by_file: BTreeMap<&str, BTreeMap<u32, &FunctionEntry>> = BTreeMap::new();
    for e in entries {
        by_file.entry(e.file.as_str()).or_default().insert(e.rva, e);
    }
    by_file
}

fn write_stdout(entries: &[FunctionEntry]) -> crate::Result<()> {
    let stdout = std::io::stdout();
    let mut w = stdout.lock();
    for (file, funs) in group_by_file(entries) {
        writeln!(w, "// ===== {file} =====\n")?;
        for e in funs.values() {
            writeln!(w, "{}", rich_render::render_listing(e))?;
        }
    }
    Ok(())
}

/// Write the structure-style tree: `<dir>/sources/<relative path>`, one file per
/// source file, functions in RVA order.
fn write_tree(dir: &Path, entries: &[FunctionEntry]) -> crate::Result<()> {
    let root = dir.join("sources");

    for (file, funs) in group_by_file(entries) {
        let full = root.join(file.trim_start_matches('/'));
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut out = std::io::BufWriter::new(std::fs::File::create(&full)?);
        for e in funs.values() {
            writeln!(out, "{}", rich_render::render_listing(e))?;
        }
        out.flush()?;
    }

    Ok(())
}

/// Write the queryable index: `<dir>/index.jsonl`, one JSON [`FunctionEntry`] per
/// line, sorted by (file, rva) for stable diffs. The query/fetch tools read this
/// without touching the PDB.
fn write_index(dir: &Path, entries: &[FunctionEntry]) -> crate::Result<()> {
    let mut ordered: Vec<&FunctionEntry> = entries.iter().collect();
    ordered.sort_by(|a, b| a.file.cmp(&b.file).then(a.rva.cmp(&b.rva)));

    let mut out = std::io::BufWriter::new(std::fs::File::create(dir.join("index.jsonl"))?);
    for e in ordered {
        serde_json::to_writer(&mut out, e)?;
        writeln!(out)?;
    }
    out.flush()?;

    Ok(())
}
