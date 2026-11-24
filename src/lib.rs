//! Python `ctypes` emitter for the Handl codegen pipeline (ADR-0107,
//! PL-004 T-075).
//!
//! Consumes the generic [`SdkgenModule`] IR and materializes one
//! `<module>.py` per module: `ctypes.Structure` classes for records,
//! aliases for typedefs, ints for enum/macro constants, and functions
//! bound to the shared library with `argtypes`/`restype` set from the
//! marshal descriptors. This is a faithful C-ABI projection — the
//! Handl port/adapter pair stays the ergonomic surface.
//!
//! Three targets register here:
//! - `python` — `Source "c"` (resolved by name) + [`PythonEmitter`]
//!   over `SdkgenModule`, for C-header inputs.
//! - `python-spec` — [`PythonSpecSource`] + [`PythonSpecEmitter`] over
//!   [`SpecModule`], for `.yaml`/`.yml`/`.json` spec inputs (the
//!   embedded-CPython shim path — PL-004 replaced the IR-free
//!   `spec_input` bypass with a real source/emitter pairing).
//! - `python-import` — [`PythonImportSource`] +
//!   [`PythonImportEmitter`] over [`SpecModule`], the *read*
//!   direction: shell out to the vendored `scripts/hnl-introspect.py`
//!   (inspect/typing must run inside Python), parse its JSON into the
//!   spec document, and emit `<module>/<module>.hnl` — `ffi.call` into
//!   libpython with a JSON env-var value channel. v1 covers
//!   module-level functions with scalar/`List(T)` signatures; every
//!   skipped symbol is reported, never dropped.

use handl_plugin_codegen::{
    Change, CodegenEmitterDef, CodegenError, CodegenRegistry, CodegenRequest, CodegenSourceDef,
    CodegenTarget, Emitter, Function, Record, SdkgenModule, Source, SpecModule, erase_emitter,
    erase_source,
};
use std::collections::HashSet;

mod import;
mod spec;

pub use import::{
    IMPORT_FORMAT, INTROSPECT_SCRIPT, ImportFunction, ImportParam, ImportSurface,
    PythonImportEmitter, PythonImportSource, build_hnl, map_ann, surface_from_document,
};
pub use spec::is_python_spec;

/// Register the `python` codegen targets on `reg` (ADR-0128 §0.1/§0.2,
/// PL-004):
///
/// - `python` — source `"c"` + emitter `"python"` (the `SdkgenModule`
///   ctypes projection; `source_filters` keeps `--include`/`--exclude`).
///   The source id resolves *by name* through the registry — `python/`
///   depends on `c/`'s module at resolution time, never at the crate
///   level.
/// - `python-spec` — the spec-driven `.hnl` shim path over `SpecModule`.
///   It is a separate target because one target pairs exactly one IR
///   family; input-kind routing (`python` vs `python-spec` for a yaml
///   spec) is the command layer's job (T-076).
pub fn register(reg: &mut CodegenRegistry) {
    reg.register_emitter(CodegenEmitterDef {
        name: "python",
        make: make_python_emitter,
    });
    reg.register_source(CodegenSourceDef {
        name: "python-spec",
        make: make_spec_source,
    });
    reg.register_emitter(CodegenEmitterDef {
        name: "python-spec",
        make: make_spec_emitter,
    });
    reg.register_target(CodegenTarget {
        name: "python",
        source: "c",
        emitter: "python",
        source_filters: true,
        // The ctypes projection does not layer external docs — unlike
        // the `c`/`webidl` targets, no doc pass runs post-parse.
        docs: None,
        spec_input: None,
    });
    reg.register_target(CodegenTarget {
        name: "python-spec",
        source: "python-spec",
        emitter: "python-spec",
        source_filters: false,
        docs: None,
        spec_input: None,
    });
    // `python-import` — the read direction: a python module/package
    // (path, package dir, or dotted name) → `<module>/<module>.hnl`
    // over embedded CPython. The source probes via subprocess (the
    // cli backend's `--help` pattern), so `source_filters` stays off.
    reg.register_source(CodegenSourceDef {
        name: "python-import",
        make: import::make_import_source,
    });
    reg.register_emitter(CodegenEmitterDef {
        name: "python-import",
        make: import::make_import_emitter,
    });
    reg.register_target(CodegenTarget {
        name: "python-import",
        source: "python-import",
        emitter: "python-import",
        source_filters: false,
        docs: None,
        spec_input: None,
    });
}

fn make_python_emitter(
    _req: &CodegenRequest<'_>,
) -> Result<Box<dyn handl_plugin_codegen::AnyEmitter>, String> {
    Ok(erase_emitter(PythonEmitter::new()))
}

fn make_spec_source(
    _req: &CodegenRequest<'_>,
) -> Result<Box<dyn handl_plugin_codegen::AnySource>, String> {
    Ok(erase_source(PythonSpecSource))
}

fn make_spec_emitter(
    _req: &CodegenRequest<'_>,
) -> Result<Box<dyn handl_plugin_codegen::AnyEmitter>, String> {
    Ok(erase_emitter(PythonSpecEmitter))
}

/// `Source<SpecModule>` for the spec path — the yaml/json loader is
/// [`SpecModule::load`], shared with every spec-family target.
pub struct PythonSpecSource;

impl Source<SpecModule> for PythonSpecSource {
    fn parse(&self, req: &handl_plugin_codegen::EmitContext) -> Result<SpecModule, CodegenError> {
        SpecModule::load(
            req.spec_path.as_deref().ok_or_else(|| {
                CodegenError::Parse("python-spec source requires EmitContext::spec_path".into())
            })?,
            req.source_url.clone(),
        )
    }
}

/// `Emitter<SpecModule>` rendering the `<package>/<package>.hnl`
/// embedded-CPython shim — pure `materialize`, `Change`-only output.
pub struct PythonSpecEmitter;

impl Emitter<SpecModule> for PythonSpecEmitter {
    fn materialize(
        &self,
        module: &SpecModule,
        _req: &handl_plugin_codegen::EmitContext,
    ) -> Result<Vec<Change>, CodegenError> {
        let (artifact, content) = spec::python_hnl_artifact(&module.document)?;
        Ok(vec![Change::text(artifact, content)])
    }
}

/// Emitter that materializes Python `ctypes` bindings as `Change`s.
pub struct PythonEmitter;

impl PythonEmitter {
    pub fn new() -> Self {
        Self
    }
}

impl Default for PythonEmitter {
    fn default() -> Self {
        Self::new()
    }
}

impl Emitter<SdkgenModule> for PythonEmitter {
    /// One `<module>.py` artifact — no fs writes (PL-004).
    fn materialize(
        &self,
        module: &SdkgenModule,
        _req: &handl_plugin_codegen::EmitContext,
    ) -> Result<Vec<Change>, CodegenError> {
        let name = if module.module.is_empty() {
            "module"
        } else {
            &module.module
        };
        Ok(vec![Change::text(
            format!("{name}.py"),
            build_module(module),
        )])
    }
}

/// The whole generated file.
pub fn build_module(module: &SdkgenModule) -> String {
    let mut out = String::new();
    let name = if module.module.is_empty() {
        "module"
    } else {
        &module.module
    };

    out.push_str(&format!(
        "# Generated by `hnl codegen python` from {name} — do not edit.\n"
    ));
    out.push_str(&format!(
        "# backend: {}  target: {}\n",
        module.backend_id, module.target
    ));
    if let Some(src) = &module.source {
        out.push_str(&format!("# source: {src}\n"));
    }
    out.push_str(&format!("\n\"\"\"ctypes bindings for {name}.\"\"\"\n\n"));
    out.push_str("import ctypes\n\n");

    let lib = if module.library.is_empty() {
        "None".to_string()
    } else {
        format!("\"{}\"", module.library)
    };
    out.push_str(&format!("_lib = ctypes.CDLL({lib}, use_errno=True)\n\n"));

    let record_names: HashSet<&str> = module.records.iter().map(|r| r.name.as_str()).collect();
    let mut names: Vec<String> = Vec::new();

    if !module.records.is_empty() {
        out.push_str("# --- records ---\n\n");
        for rec in &module.records {
            emit_record(&mut out, rec, &record_names);
            names.push(rec.name.clone());
        }
    }

    if !module.typedefs.is_empty() {
        out.push_str("# --- typedefs ---\n\n");
        for td in &module.typedefs {
            out.push_str(&format!(
                "{} = {}\n",
                td.name,
                ctypes_type(&td.descriptor, &record_names)
            ));
            names.push(td.name.clone());
        }
        out.push('\n');
    }

    if !module.enum_constants.is_empty() {
        out.push_str("# --- constants ---\n\n");
        for c in &module.enum_constants {
            out.push_str(&format!("{} = {}\n", c.name, c.value));
            names.push(c.name.clone());
        }
        out.push('\n');
    }

    if !module.functions.is_empty() {
        out.push_str("# --- functions ---\n\n");
        for f in &module.functions {
            emit_function(&mut out, f, &record_names);
            names.push(f.name.clone());
        }
    }

    names.sort();
    names.dedup();
    out.push_str("__all__ = [\n");
    for n in &names {
        out.push_str(&format!("    \"{n}\",\n"));
    }
    out.push_str("]\n");
    out
}

fn emit_record(out: &mut String, rec: &Record, records: &HashSet<&str>) {
    let mut specs: Vec<FieldSpec> = Vec::new();
    for field in &rec.fields {
        let (ty, nested) = field_ctypes_type(&field.descriptor, &rec.name, &field.name, records);
        for (nested_name, nested_fields) in nested {
            out.push_str(&format!("class {nested_name}(ctypes.Structure):\n"));
            out.push_str("    _fields_ = [\n");
            for (nf_name, nf_ty) in nested_fields {
                out.push_str(&format!("        (\"{nf_name}\", {nf_ty}),\n"));
            }
            out.push_str("    ]\n\n");
        }
        specs.push(FieldSpec {
            name: field.name.clone(),
            kind: field.descriptor.clone(),
            offset: field.offset,
            width: Some(field.size),
            resolved: Some(ty),
        });
    }

    if let Some(doc) = &rec.doc {
        out.push_str(&format!("\"\"\"{}\"\"\"\n", doc.replace('"', "'")));
    }
    emit_struct_class(out, &rec.name, &specs, records, Some(rec.size));
}

/// A record shape the emitter synthesizes from a function's inline
/// struct descriptor (`in_struct:{…}`) — the IR carries no `Record` for
/// it, exactly as the Handl file emitter's `<Port>Msg` shapes.
struct SynthRecord {
    name: String,
    fields: Vec<FieldSpec>,
}

/// One parsed field of a struct descriptor spec: `name:kind@offset[:width]`.
struct FieldSpec {
    name: String,
    kind: String,
    offset: usize,
    width: Option<usize>,
    /// A ctypes type already resolved by the caller (record fields carry
    /// nested-struct class names); `kind` is mapped instead when `None`.
    resolved: Option<String>,
}

/// The `{…}` body of an inline struct descriptor (`in_struct:{…}`,
/// `out_struct:{…}`, `io_struct:{…}`), if `desc` is one.
fn inline_struct_spec(desc: &str) -> Option<&str> {
    for prefix in ["in_struct:", "out_struct:", "io_struct:"] {
        if let Some(rest) = desc.strip_prefix(prefix)
            && let Some(inner) = rest.strip_prefix('{')
        {
            return inner.strip_suffix('}');
        }
    }
    None
}

/// Parse a struct descriptor body into field specs. Brace-aware: a
/// nested `rec:{…}` kind stays in one piece.
fn parse_field_specs(body: &str) -> Vec<FieldSpec> {
    let mut out = Vec::new();
    let mut depth = 0usize;
    let mut current = String::new();
    for ch in body.chars() {
        match ch {
            '{' => {
                depth += 1;
                current.push(ch);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if depth == 0 => {
                if let Some(spec) = parse_one_field(&current) {
                    out.push(spec);
                }
                current.clear();
            }
            _ => current.push(ch),
        }
    }
    if let Some(spec) = parse_one_field(&current) {
        out.push(spec);
    }
    out
}

fn parse_one_field(text: &str) -> Option<FieldSpec> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let (name, rest) = text.split_once(':')?;
    let (kind, layout) = match rest.split_once('@') {
        Some((k, l)) => (k, Some(l)),
        None => (rest, None),
    };
    let mut offset = 0usize;
    let mut width = None;
    if let Some(l) = layout {
        let mut parts = l.splitn(2, ':');
        offset = parts
            .next()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        width = parts.next().and_then(|s| s.trim().parse().ok());
    }
    Some(FieldSpec {
        name: name.trim().to_string(),
        kind: kind.trim().to_string(),
        offset,
        width,
        resolved: None,
    })
}

/// Byte width of a descriptor slot — the spec's explicit width wins,
/// otherwise the scalar's own width. Unknown kinds assume a pointer.
fn desc_size(desc: &str, explicit: Option<usize>) -> usize {
    if let Some(w) = explicit {
        return w;
    }
    match desc {
        "i8" | "u8" => 1,
        "i16" | "u16" => 2,
        "i32" | "u32" | "f32" => 4,
        "i64" | "u64" | "f64" | "isize" | "usize" => 8,
        _ => 8,
    }
}

/// Emit a `ctypes.Structure` that reproduces the declared offsets exactly.
/// `_pack_ = 1` makes ctypes lay fields out contiguously, so a gap before
/// a field becomes an explicit `c_ubyte * n` pad and any layout — packed
/// or naturally aligned — is reproduced without guessing. Every offset is
/// asserted at import; `total_size` adds the trailing pad and a size
/// assertion when the caller knows the C size.
fn emit_struct_class(
    out: &mut String,
    name: &str,
    fields: &[FieldSpec],
    records: &HashSet<&str>,
    total_size: Option<usize>,
) {
    out.push_str(&format!("class {name}(ctypes.Structure):\n"));
    // `_pack_ = 1` lays fields out contiguously so the explicit pads below
    // reproduce any C layout exactly. `_layout_ = "ms"` pins that packing
    // to the MSVC model, which is what bare `_pack_` already meant and
    // what CPython 3.19+ will require (3.14+ rejects `_pack_` under the
    // default gcc-sysv layout).
    out.push_str("    _layout_ = \"ms\"\n");
    out.push_str("    _pack_ = 1\n");
    out.push_str("    _fields_ = [\n");
    let mut cursor = 0usize;
    for (i, f) in fields.iter().enumerate() {
        if f.offset > cursor {
            out.push_str(&format!(
                "        (\"_pad_{i}\", ctypes.c_ubyte * {}),\n",
                f.offset - cursor
            ));
            cursor = f.offset;
        }
        out.push_str(&format!(
            "        (\"{}\", {}),\n",
            f.name,
            f.resolved
                .clone()
                .unwrap_or_else(|| ctypes_type(&f.kind, records))
        ));
        cursor += desc_size(&f.kind, f.width);
    }
    if let Some(total) = total_size
        && total > cursor
    {
        out.push_str(&format!(
            "        (\"_pad_tail\", ctypes.c_ubyte * {}),\n",
            total - cursor
        ));
    }
    out.push_str("    ]\n");
    for f in fields {
        out.push_str(&format!(
            "assert {name}.{}.offset == {}, \"{name}.{}.offset\"\n",
            f.name, f.offset, f.name
        ));
    }
    if let Some(total) = total_size {
        out.push_str(&format!(
            "assert ctypes.sizeof({name}) == {total}, \"{name}.__sizeof__\"\n"
        ));
    }
    out.push('\n');
}

/// The synthesized `<Port>Msg` shape for a function whose descriptors
/// carry an inline struct, if any.
fn synth_record(f: &Function) -> Option<SynthRecord> {
    let spec = f
        .arg_descriptors
        .iter()
        .find_map(|d| inline_struct_spec(d))?;
    let fields = parse_field_specs(spec);
    if fields.is_empty() {
        return None;
    }
    Some(SynthRecord {
        name: format!("{}Msg", f.port_name),
        fields,
    })
}

fn emit_function(out: &mut String, f: &Function, records: &HashSet<&str>) {
    let synth = synth_record(f);
    let msg_name = synth.as_ref().map(|s| s.name.as_str());
    let argtypes: Vec<String> = f
        .arg_descriptors
        .iter()
        .map(|d| match (inline_struct_spec(d), msg_name) {
            (Some(_), Some(msg)) => format!("ctypes.POINTER({msg})"),
            _ => ctypes_type(d, records),
        })
        .collect();
    let restype = ctypes_type(&f.ret_descriptor, records);

    if let Some(s) = &synth {
        out.push_str(&format!("# {}'s inline struct argument\n", f.port_name));
        emit_struct_class(out, &s.name, &s.fields, records, None);
    }
    if let Some(doc) = &f.doc {
        out.push_str(&format!("# {}\n", doc.replace('\n', " ")));
    }
    out.push_str(&format!("# handl port: {}\n", port_signature(f)));
    out.push_str(&format!(
        "_lib.{}.argtypes = [{}]\n",
        f.name,
        argtypes.join(", ")
    ));
    out.push_str(&format!("_lib.{}.restype = {restype}\n", f.name));

    let params: Vec<String> = if f.port_args.len() == f.arg_descriptors.len() {
        f.port_args.iter().map(|a| a.name.clone()).collect()
    } else {
        (0..f.arg_descriptors.len())
            .map(|i| format!("a{i}"))
            .collect()
    };
    let call_args = params.join(", ");
    out.push_str(&format!("def {}({call_args}):\n", f.name));
    if let Some(doc) = &f.doc {
        out.push_str(&format!("    \"\"\"{}\"\"\"\n", doc.replace('"', "'")));
    }
    let guarded = f.errno_guard && f.ret_descriptor != "void" && !f.ret_descriptor.is_empty();
    if guarded {
        // Pointer-returning calls signal failure with NULL, not a negative
        // value (`SSL_new(NULL)` returns NULL); integers use `< 0`.
        let is_ptr = matches!(restype.as_str(), "ctypes.c_void_p" | "ctypes.c_char_p")
            || restype.starts_with("ctypes.POINTER(");
        out.push_str(&format!("    _r = _lib.{}({call_args})\n", f.name));
        if is_ptr {
            out.push_str("    if not _r:\n");
        } else {
            out.push_str("    if _r < 0:\n");
        }
        out.push_str("        return (False, ctypes.get_errno())\n");
        out.push_str("    return (True, _r)\n\n");
    } else {
        out.push_str(&format!(
            "    return (True, _lib.{}({call_args}))\n\n",
            f.name
        ));
    }
}

/// The Handl port signature, as a comment for the reader.
fn port_signature(f: &Function) -> String {
    let args = f
        .port_args
        .iter()
        .map(|a| format!("{}: {}", a.name, a.port))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{}: ({args}) -> {}", f.port_name, f.return_port)
}

/// The ctypes type for a marshal descriptor. Unknown descriptors degrade
/// to `ctypes.c_void_p` — the ABI slot is still right.
fn ctypes_type(desc: &str, records: &HashSet<&str>) -> String {
    let scalar = |t: &str| match t {
        "i8" => Some("ctypes.c_int8"),
        "u8" => Some("ctypes.c_uint8"),
        "i16" => Some("ctypes.c_int16"),
        "u16" => Some("ctypes.c_uint16"),
        "i32" => Some("ctypes.c_int32"),
        "u32" => Some("ctypes.c_uint32"),
        "i64" => Some("ctypes.c_int64"),
        "u64" => Some("ctypes.c_uint64"),
        "isize" => Some("ctypes.c_ssize_t"),
        "usize" => Some("ctypes.c_size_t"),
        "f32" => Some("ctypes.c_float"),
        "f64" => Some("ctypes.c_double"),
        _ => None,
    };
    if let Some(t) = scalar(desc) {
        return t.to_string();
    }
    if let Some(rest) = desc.strip_prefix("io_scalar:") {
        return format!("ctypes.POINTER({})", ctypes_type(rest, records));
    }
    if let Some(rest) = desc.strip_prefix("rec:") {
        return if records.contains(rest) {
            format!("ctypes.POINTER({rest})")
        } else {
            "ctypes.c_void_p".to_string()
        };
    }
    if let Some(rest) = desc.strip_prefix("out_struct:") {
        return if records.contains(rest) {
            format!("ctypes.POINTER({rest})")
        } else {
            "ctypes.c_void_p".to_string()
        };
    }
    if let Some(rest) = desc.strip_prefix("arr:") {
        // arr:N:T — inline array of T
        let mut parts = rest.splitn(2, ':');
        let n = parts.next().unwrap_or("0");
        let t = parts.next().unwrap_or("u8");
        return format!("{} * {n}", ctypes_type(t, records));
    }
    match desc {
        "void" => "None".to_string(),
        "cstr" => "ctypes.c_char_p".to_string(),
        _ => "ctypes.c_void_p".to_string(),
    }
}

/// A record field's ctypes type. Inline nested structs (`rec:{...}`)
/// come back as a generated anonymous field entry.
#[allow(clippy::type_complexity)]
fn field_ctypes_type(
    desc: &str,
    parent: &str,
    field: &str,
    records: &HashSet<&str>,
) -> (String, Vec<(String, Vec<(String, String)>)>) {
    if let Some(spec) = desc.strip_prefix("rec:")
        && spec.starts_with('{')
    {
        let inner = spec.trim_start_matches('{').trim_end_matches('}');
        let fields = inner
            .split(',')
            .filter(|s| !s.trim().is_empty())
            .filter_map(|s| {
                let mut parts = s.splitn(2, ':');
                let name = parts.next()?.trim().to_string();
                let kind = parts.next()?.split('@').next()?.trim().to_string();
                Some((name, ctypes_type(&kind, records)))
            })
            .collect();
        let name = format!("_{parent}_{field}");
        return (name.clone(), vec![(name, fields)]);
    }
    (ctypes_type(desc, records), Vec::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use handl_plugin_codegen::{Arg, EnumConst, Field, Typedef};

    fn module() -> SdkgenModule {
        SdkgenModule {
            client_package: "ffi".into(),
            backend_id: "c".into(),
            target: "x86_64-unknown-linux-gnu".into(),
            library: "libc.so.6".into(),
            module: "epoll".into(),
            records: vec![Record {
                name: "EpollCtlMsg".into(),
                fields: vec![
                    Field {
                        name: "events".into(),
                        port: "Nat".into(),
                        descriptor: "u32".into(),
                        offset: 0,
                        size: 4,
                        ptr_depth: 0,
                        is_const: false,
                        doc: None,
                    },
                    Field {
                        name: "data".into(),
                        port: "Nat".into(),
                        descriptor: "u64".into(),
                        offset: 4,
                        size: 8,
                        ptr_depth: 0,
                        is_const: false,
                        doc: None,
                    },
                ],
                size: 12,
                align: 1,
                doc: None,
            }],
            functions: vec![Function {
                name: "epoll_create1".into(),
                port_name: "EpollCreate1".into(),
                port_args: vec![Arg {
                    name: "flags".into(),
                    port: "Int".into(),
                }],
                arg_descriptors: vec!["i32".into()],
                return_port: "Result<Int, Errno>".into(),
                ret_descriptor: "i32".into(),
                out_struct: None,
                variadic: false,
                errno_guard: true,
                c_decl: "int epoll_create1(int flags);".into(),
                doc: Some("Same as epoll_create but with FLAGS.".into()),
            }],
            typedefs: vec![Typedef {
                name: "Socklen".into(),
                port: "Socklen".into(),
                descriptor: "u32".into(),
                doc: None,
            }],
            enum_constants: vec![EnumConst {
                name: "EPOLLIN".into(),
                value: 1,
                doc: None,
            }],
            ..Default::default()
        }
    }

    #[test]
    fn emits_library_and_constants() {
        let out = build_module(&module());
        assert!(out.contains("_lib = ctypes.CDLL(\"libc.so.6\", use_errno=True)"));
        assert!(out.contains("EPOLLIN = 1"));
        assert!(out.contains("Socklen = ctypes.c_uint32"));
    }

    #[test]
    fn emits_packed_record_with_layout_asserts() {
        let out = build_module(&module());
        assert!(out.contains("class EpollCtlMsg(ctypes.Structure):"));
        assert!(out.contains("_pack_ = 1"));
        assert!(out.contains("(\"events\", ctypes.c_uint32),"));
        assert!(out.contains("(\"data\", ctypes.c_uint64),"));
        assert!(out.contains("assert ctypes.sizeof(EpollCtlMsg) == 12"));
        assert!(out.contains("assert EpollCtlMsg.data.offset == 4"));
    }

    #[test]
    fn padded_layout_gets_explicit_pad_fields() {
        let mut m = module();
        m.records[0].fields[1].offset = 8;
        m.records[0].size = 24;
        let out = build_module(&m);
        // gap 4..8 becomes an explicit pad; trailing 16..24 too
        assert!(out.contains("(\"_pad_1\", ctypes.c_ubyte * 4),"));
        assert!(out.contains("(\"_pad_tail\", ctypes.c_ubyte * 8),"));
        assert!(out.contains("assert EpollCtlMsg.data.offset == 8"));
        assert!(out.contains("assert ctypes.sizeof(EpollCtlMsg) == 24"));
    }

    #[test]
    fn inline_struct_arg_gets_synth_msg_class() {
        let mut m = module();
        m.functions[0].arg_descriptors = vec!["in_struct:{events:u32@0,data:u64@4}".into()];
        let out = build_module(&m);
        assert!(out.contains("class EpollCreate1Msg(ctypes.Structure):"));
        assert!(out.contains("_lib.epoll_create1.argtypes = [ctypes.POINTER(EpollCreate1Msg)]"));
        assert!(out.contains("assert EpollCreate1Msg.data.offset == 4"));
    }

    #[test]
    fn emits_errno_guarded_function() {
        let out = build_module(&module());
        assert!(out.contains("_lib.epoll_create1.argtypes = [ctypes.c_int32]"));
        assert!(out.contains("_lib.epoll_create1.restype = ctypes.c_int32"));
        assert!(out.contains("def epoll_create1(flags):"));
        assert!(out.contains("return (False, ctypes.get_errno())"));
    }

    #[test]
    fn void_return_skips_errno_guard() {
        let mut m = module();
        m.functions[0].ret_descriptor = "void".into();
        let out = build_module(&m);
        assert!(out.contains("_lib.epoll_create1.restype = None"));
        assert!(out.contains("return (True, _lib.epoll_create1(flags))"));
        assert!(!out.contains("ctypes.get_errno()"));
    }

    #[test]
    fn pointer_return_guards_on_null() {
        let mut m = module();
        m.functions[0].ret_descriptor = "handle".into();
        m.functions[0].return_port = "Handle".into();
        let out = build_module(&m);
        assert!(out.contains("_lib.epoll_create1.restype = ctypes.c_void_p"));
        assert!(out.contains("    if not _r:\n"));
        assert!(!out.contains("if _r < 0"));
    }

    #[test]
    fn buffer_descriptors_degrade_to_void_p() {
        let mut m = module();
        m.functions[0].arg_descriptors = vec!["out_ptr:8".into(), "in_buf".into()];
        m.functions[0].port_args = vec![Arg {
            name: "flags".into(),
            port: "Int".into(),
        }];
        let out = build_module(&m);
        // descriptor count != port arg count -> positional names
        assert!(out.contains("_lib.epoll_create1.argtypes = [ctypes.c_void_p, ctypes.c_void_p]"));
        assert!(out.contains("def epoll_create1(a0, a1):"));
    }

    #[test]
    fn inline_nested_record_field() {
        let mut m = module();
        m.records[0].fields.push(Field {
            name: "inner".into(),
            port: "Handle".into(),
            descriptor: "rec:{x:i32@0,y:i32@4}".into(),
            offset: 12,
            size: 8,
            ptr_depth: 0,
            is_const: false,
            doc: None,
        });
        m.records[0].size = 20;
        let out = build_module(&m);
        assert!(out.contains("_EpollCtlMsg_inner"));
        assert!(out.contains("(\"x\", ctypes.c_int32),"));
    }
}
