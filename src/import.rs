//! `python-import` — the *read* direction: introspect a Python module
//! or package and emit a typed `.hnl` surface over embedded CPython.
//!
//! Pipeline shape (same `Source<I> → IrModule → Emitter<I>` contract as
//! every `tool-codegen-*` target):
//!
//! - [`PythonImportSource`] (`Source<SpecModule>`) shells out to
//!   `python3 scripts/hnl-introspect.py <spec>` — introspection must run
//!   inside Python (`inspect`/`typing`), the same subprocess-probe
//!   pattern `tool-codegen-cli` uses for `--help`. The script's JSON
//!   stdout becomes the `SpecModule` document; every symbol the script
//!   or the v1 policy rejects lands in `diagnostics` (stderr via the
//!   registry) *and* as a `-- <name>: SKIPPED` comment in the artifact.
//! - [`PythonImportEmitter`] (`Emitter<SpecModule>`) renders
//!   `<module>/<module>.hnl`: `ffi.call` into `libpython`
//!   (`Py_Initialize`/`PyRun_SimpleString`) with args/results crossing
//!   as JSON through the `HNL_PY_IN`/`HNL_PY_OUT` env vars — the spike
//!   (`/tmp/hnl-pygen`) proved this shape; this module is the honest,
//!   policy-checked version of it.
//!
//! v1 scope: module-level functions whose params/return map to
//! `Int`/`Number`/`String`/`Bool`/`List(T)` (scalar `None` returns
//! degrade to `Value`). Everything else — `Callable`, `*args`/
//! `**kwargs`, classes, async, C extensions/builtins without
//! annotations, `dict`/`tuple`/`set`/unions/`Optional`, unannotated
//! params — skips the *whole function* with a loud reason. Nothing is
//! silently widened to `Any`.
//!
//! Everything here is pure except [`PythonImportSource::parse`], which
//! runs the read-only introspection subprocess (PL-004: `parse` may
//! probe, never write).

use handl_plugin_codegen::{
    Change, CodegenError, EmitContext, Emitter, Source, SpecModule, erase_emitter, erase_source,
};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The vendored introspector — `python3 -c <this> <target>` prints the
/// surface JSON. Kept as a real file under `scripts/` so it can be run
/// standalone (`python3 scripts/hnl-introspect.py <mod>`); embedded here
/// so the source never depends on the checkout layout.
pub const INTROSPECT_SCRIPT: &str = include_str!("../scripts/hnl-introspect.py");

/// `SpecModule.format` tag for introspection documents — distinguishes
/// them from hand-authored yaml/json specs on the same IR family.
pub const IMPORT_FORMAT: &str = "python-import";

/// Per-invocation cap for the introspection subprocess. Imports can
/// pull in heavy packages; generous but bounded (the cli probe uses 10s
/// for `--help`, which never imports user code).
const INTROSPECT_TIMEOUT: Duration = Duration::from_secs(60);

// ------------------------------------------------------------------
// Source — subprocess probe → SpecModule
// ------------------------------------------------------------------

/// `Source<SpecModule>` for `python-import`: resolve the Python
/// interpreter, run [`INTROSPECT_SCRIPT`] against `spec_path`, parse
/// the JSON document, and collect skip diagnostics.
pub struct PythonImportSource;

impl Source<SpecModule> for PythonImportSource {
    fn parse(&self, req: &EmitContext) -> Result<SpecModule, CodegenError> {
        let spec = req.spec_path.as_deref().ok_or_else(|| {
            CodegenError::Parse("python-import source requires EmitContext:=spec_path".into())
        })?;
        let python = python_executable(req)?;
        let document = run_introspect(&python, spec)?;

        let module_name = document
            .get("module")
            .and_then(|m| m.as_str())
            .unwrap_or("module")
            .to_string();
        let diagnostics = skip_diagnostics(&document);

        Ok(SpecModule {
            format: IMPORT_FORMAT.to_string(),
            name: module_name,
            document,
            source: req.source_url.clone(),
            diagnostics,
        })
    }
}

/// Pick the interpreter: `--config python=<exe>`-style `EmitContext`
/// config, then `$HNL_PYTHON`, then `python3`/`python` on PATH.
fn python_executable(req: &EmitContext) -> Result<String, CodegenError> {
    if let Some(p) = req.config.get("python") {
        return Ok(p.clone());
    }
    if let Ok(p) = std::env::var("HNL_PYTHON")
        && !p.is_empty()
    {
        return Ok(p);
    }
    for cand in ["python3", "python"] {
        let ok = Command::new(cand)
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            return Ok(cand.to_string());
        }
    }
    Err(CodegenError::Parse(
        "python-import: no python interpreter found (tried python3, python; \
         set HNL_PYTHON or pass config python=<exe>)"
            .into(),
    ))
}

/// `python3 -c INTROSPECT_SCRIPT <spec>` → parsed JSON document. Any
/// nonzero exit or non-JSON stdout is a `CodegenError::Parse` carrying
/// the interpreter's stderr tail — introspection failures are loud.
fn run_introspect(python: &str, spec: &Path) -> Result<serde_json::Value, CodegenError> {
    let mut child = Command::new(python)
        .arg("-c")
        .arg(INTROSPECT_SCRIPT)
        .arg(spec.as_os_str())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| CodegenError::Parse(format!("python-import: spawn `{python}` failed: {e}")))?;

    // Drain both pipes on threads — a large module's JSON can exceed
    // the pipe buffer, and a child blocked on write would never exit
    // (the cli probe gets away with post-exit reads because `--help`
    // output is tiny; introspection JSON is not).
    let mut out_pipe = child.stdout.take().expect("piped stdout");
    let mut err_pipe = child.stderr.take().expect("piped stderr");
    let out_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        buf
    });
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + INTROSPECT_TIMEOUT;
    let status = loop {
        // A failed poll is retried until the deadline rather than
        // failing the probe on a transient wait error (probe.rs
        // convention).
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(_) => {}
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            let _ = out_thread.join();
            let _ = err_thread.join();
            return Err(CodegenError::Parse(format!(
                "python-import: `{python}` introspection of {} timed out",
                spec.display()
            )));
        }
        std::thread::sleep(Duration::from_millis(10));
    };

    let stdout_bytes = out_thread
        .join()
        .map_err(|_| CodegenError::Parse("python-import: stdout reader panicked".into()))?;
    let stderr_bytes = err_thread
        .join()
        .map_err(|_| CodegenError::Parse("python-import: stderr reader panicked".into()))?;
    let stdout = String::from_utf8_lossy(&stdout_bytes);
    let stderr = String::from_utf8_lossy(&stderr_bytes);
    if !status.success() {
        let tail: String = stderr.lines().rev().take(8).collect::<Vec<_>>().join("\n");
        return Err(CodegenError::Parse(format!(
            "python-import: introspection of {} failed ({python}):\n{tail}",
            spec.display()
        )));
    }
    serde_json::from_str(stdout.trim()).map_err(|e| {
        CodegenError::Parse(format!(
            "python-import: introspection of {} produced non-JSON output: {e}\n{stdout}",
            spec.display()
        ))
    })
}

/// One `diagnostics` line per symbol the introspector already rejected
/// (async, classes, signature-less builtins). Policy-level skips are
/// computed by [`surface_from_document`] and land in the artifact
/// comments — the registry prints `diagnostics` before materialize, so
/// both channels stay loud.
fn skip_diagnostics(document: &serde_json::Value) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(fns) = document.get("functions").and_then(|f| f.as_array()) {
        for f in fns {
            if let Some(reason) = f.get("skip").and_then(|s| s.as_str()) {
                let name = f.get("name").and_then(|n| n.as_str()).unwrap_or("?");
                out.push(format!("python-import: `{name}` skipped — {reason}"));
            }
        }
    }
    out
}

// ------------------------------------------------------------------
// IR — the typed surface parsed out of the introspection document
// ------------------------------------------------------------------

/// A parsed introspection document: everything the emitter needs,
/// already classified (supported vs skipped-with-reason).
#[derive(Debug, Clone)]
pub struct ImportSurface {
    /// Module/package name (`toymod`, `pkg.sub`).
    pub module: String,
    /// `sysconfig` LDLIBRARY (`libpython3.14.so`) or a fallback guess.
    pub libpython: String,
    /// Absolute path the module was loaded from, when known — the
    /// dispatcher reproduces the import from it at runtime.
    pub module_file: Option<String>,
    /// Extra `sys.path` entries the introspector prepended — replayed
    /// inside the generated dispatcher so `import_module` resolves the
    /// same way.
    pub path_entries: Vec<String>,
    /// Every public callable/class record, supported or skipped.
    pub functions: Vec<ImportFunction>,
}

/// One surface item after v1 classification.
#[derive(Debug, Clone)]
pub struct ImportFunction {
    /// hnl-safe export name (`.` → `_`).
    pub name: String,
    /// The attribute name on the Python module (`getattr(_m, py_name)`).
    pub py_name: String,
    pub doc: String,
    /// `Some(reason)` → rendered as `-- name: SKIPPED — reason`, never
    /// exported.
    pub skip: Option<String>,
    /// Supported params (empty when skipped).
    pub params: Vec<ImportParam>,
    /// hnl return type when supported.
    pub ret: String,
    /// Non-fatal caveats (defaults, `None`→`Value` degradation).
    pub notes: Vec<String>,
}

/// A supported parameter.
#[derive(Debug, Clone)]
pub struct ImportParam {
    pub name: String,
    /// hnl type spelling: `Int`, `Number`, `String`, `Bool`, `List(T)`.
    pub hnl: String,
    /// Python-side default exists — hnl signatures have no defaults;
    /// callers must pass explicitly (recorded as a note).
    pub has_default: bool,
}

/// Parse + classify an introspection `document` (the `SpecModule`
/// payload). Pure — the emitter and tests share this.
pub fn surface_from_document(document: &serde_json::Value) -> Result<ImportSurface, CodegenError> {
    let functions = document
        .get("functions")
        .and_then(|f| f.as_array())
        .ok_or_else(|| {
            CodegenError::Parse(
                "python-import document has no `functions` array — is this an \
                 hnl-introspect JSON payload?"
                    .into(),
            )
        })?;

    let module = document
        .get("module")
        .and_then(|m| m.as_str())
        .unwrap_or("module")
        .to_string();
    let libpython = document
        .get("libpython")
        .and_then(|l| l.as_str())
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| "libpython3.so".to_string());
    let module_file = document
        .get("module_file")
        .and_then(|m| m.as_str())
        .map(str::to_string);
    let path_entries = document
        .get("path_entries")
        .and_then(|p| p.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    let mut out = Vec::with_capacity(functions.len());
    for f in functions {
        out.push(classify_function(f)?);
    }
    Ok(ImportSurface {
        module,
        libpython,
        module_file,
        path_entries,
        functions: out,
    })
}

fn classify_function(f: &serde_json::Value) -> Result<ImportFunction, CodegenError> {
    let name = f
        .get("name")
        .and_then(|n| n.as_str())
        .unwrap_or("?")
        .to_string();
    let hnl_name = name.replace('.', "_");
    let py_name = f
        .get("py_name")
        .and_then(|n| n.as_str())
        .unwrap_or(&name)
        .to_string();
    let doc = f
        .get("doc")
        .and_then(|d| d.as_str())
        .unwrap_or("")
        .to_string();

    // Introspection-level skips (async, classes, no signature) pass
    // straight through.
    if let Some(reason) = f.get("skip").and_then(|s| s.as_str()) {
        return Ok(ImportFunction {
            name: hnl_name,
            py_name,
            doc,
            skip: Some(reason.to_string()),
            params: Vec::new(),
            ret: String::new(),
            notes: Vec::new(),
        });
    }

    let raw_params = f
        .get("params")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    let ret_ann = f
        .get("ret")
        .and_then(|r| r.as_str())
        .unwrap_or("unannotated");

    // v1 policy: any unmappable piece skips the whole function — loudly,
    // with every contributing reason listed.
    let mut reasons: Vec<String> = Vec::new();
    let mut params: Vec<ImportParam> = Vec::new();
    let mut notes: Vec<String> = Vec::new();

    for p in &raw_params {
        let pname = p.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        let kind = p
            .get("kind")
            .and_then(|k| k.as_str())
            .unwrap_or("positional");
        let ann = p
            .get("ann")
            .and_then(|a| a.as_str())
            .unwrap_or("unannotated");
        let has_default = p
            .get("has_default")
            .and_then(|d| d.as_bool())
            .unwrap_or(false);

        if kind == "var_positional" || kind == "var_keyword" {
            reasons.push(format!(
                "param `{pname}` is {} — hnl signatures have no variadic form",
                if kind == "var_positional" {
                    "*args"
                } else {
                    "**kwargs"
                }
            ));
            continue;
        }
        match map_ann(ann) {
            Ok(hnl) => {
                if has_default {
                    notes.push(format!(
                        "param `{pname}` has a Python default — hnl callers must pass it explicitly"
                    ));
                }
                params.push(ImportParam {
                    name: pname.to_string(),
                    hnl,
                    has_default,
                });
            }
            Err(why) => reasons.push(format!("param `{pname}`: {why}")),
        }
    }

    let ret = match map_ann(ret_ann) {
        Ok(hnl) => hnl,
        Err(why) => {
            reasons.push(format!("return: {why}"));
            String::new()
        }
    };

    if !reasons.is_empty() {
        return Ok(ImportFunction {
            name: hnl_name,
            py_name,
            doc,
            skip: Some(reasons.join("; ")),
            params: Vec::new(),
            ret: String::new(),
            notes: Vec::new(),
        });
    }

    if ret_ann == "none" {
        notes.push("returns None — crosses JSON as null; typed `Value` (no Void in the signature vocabulary)".to_string());
    }

    Ok(ImportFunction {
        name: hnl_name,
        py_name,
        doc,
        skip: None,
        params,
        ret,
        notes,
    })
}

/// Canonical annotation string → hnl signature type. `Err` is the loud
/// skip reason (never a silent `Any`).
///
/// Supported in v1: `int`→`Int`, `number`→`Number`, `string`→`String`,
/// `bool`→`Bool`, `none`→`Value` (return position only — enforced by
/// the caller noting it), `list[T]`→`List(T)` recursively.
/// Everything else — `dict`, `tuple`, `set`, `callable`, `class:*`,
/// `union`, `optional:*`, `bytes`, `unannotated`, `str:*`, `unknown:*`
/// — is a skip reason.
pub fn map_ann(ann: &str) -> Result<String, String> {
    if let Some(hnl) = match ann {
        "int" => Some("Int"),
        "number" => Some("Number"),
        "string" => Some("String"),
        "bool" => Some("Bool"),
        "none" => Some("Value"),
        _ => None,
    } {
        return Ok(hnl.to_string());
    }
    if let Some(inner) = ann.strip_prefix("list[")
        && let Some(inner) = inner.strip_suffix(']')
    {
        return map_ann(inner).map(|t| format!("List({t})"));
    }
    let reason = match ann {
        "dict" => "dict has no hnl signature type (JSON object; v1 skips)",
        "tuple" | "set" => "tuple/set have no hnl signature type (v1 skips)",
        "callable" => "callable cannot cross the JSON channel (fn values are not data)",
        "union" => "union type has no hnl signature form",
        "bytes" => "bytes have no hnl signature type (v1 skips)",
        "unannotated" => "unannotated — no static type (builtins/C extensions land here)",
        _ if ann.starts_with("optional:") => {
            "Optional[T] has no verified hnl signature type (v1 skips)"
        }
        _ if ann.starts_with("class:") => {
            "class instance needs a record/decode spec (v1: module-level functions only)"
        }
        _ if ann.starts_with("str:") => "unresolved string annotation (needs eval/typeshed)",
        _ if ann.starts_with("unknown:") => "unmapped annotation",
        _ => "unmapped annotation",
    };
    Err(reason.to_string())
}

// ------------------------------------------------------------------
// Emitter — ImportSurface → <module>/<module>.hnl
// ------------------------------------------------------------------

/// `Emitter<SpecModule>` rendering the `.hnl` embedded-CPython surface.
pub struct PythonImportEmitter;

impl Emitter<SpecModule> for PythonImportEmitter {
    fn materialize(
        &self,
        module: &SpecModule,
        _req: &EmitContext,
    ) -> Result<Vec<Change>, CodegenError> {
        let surface = surface_from_document(&module.document)?;
        let pkg = sanitize_ident(&surface.module);
        let artifact = format!("{pkg}/{pkg}.hnl");
        Ok(vec![Change::text(artifact, build_hnl(&surface))])
    }
}

/// `[A-Za-z0-9_]` kept, everything else → `_`; empty → `module`.
fn sanitize_ident(s: &str) -> String {
    let out: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        "module".to_string()
    } else {
        out
    }
}

/// The whole generated `.hnl` file. Pure.
pub fn build_hnl(surface: &ImportSurface) -> String {
    let mut out = String::new();
    let pkg = sanitize_ident(&surface.module);

    out.push_str(&format!(
        "--- {pkg} -- hnl surface over python module `{}`.\n",
        surface.module
    ));
    out.push_str("---\n");
    out.push_str("--- Generated by `hnl codegen python-import` (hnl-introspect.py:\n");
    out.push_str("--- inspect/get_type_hints). Embedded CPython via ffi.call; args\n");
    out.push_str("--- ride inside the py_run source as a raw triple-quoted JSON\n");
    out.push_str("--- literal (env vars cannot cross INTO python — CPython\n");
    out.push_str("--- snapshots os.environ at Py_Initialize); the result returns\n");
    out.push_str("--- through HNL_PY_OUT (os.environ write = putenv = live).\n");
    out.push_str("--- Requires `json.stringify`/`json.parse` (lib-json).\n");
    out.push_str("--- v1: module-level functions, scalar + List(T) signatures;\n");
    out.push_str("--- exports are untyped (intended types in `--` comments).\n");
    out.push_str("--- skipped symbols are annotated below, never silently dropped.\n\n");

    out.push_str("from \"ffi\" import { call }\n");
    out.push_str("from \"json\" import { stringify, parse }\n\n");

    out.push_str(&format!("PYLIB := \"{}\"\n", surface.libpython));
    out.push_str("LIBC := \"libc.so.6\"\n\n");

    // `() ~>` nullary arrows misparse when the body is a multi-arg call —
    // take a dummy param instead (`py_init(null)` at call sites).
    out.push_str("py_init := (u) ~> call(PYLIB, \"Py_Initialize\", [], [], \"void\", false)\n");
    out.push_str("py_run := (code) ~> call(PYLIB, \"PyRun_SimpleString\", [code], [\"cstr\"], \"i32\", false)\n");
    out.push_str(
        "get_env := (name) ~> call(LIBC, \"getenv\", [name], [\"cstr\"], \"cstr\", false)\n\n",
    );

    for f in &surface.functions {
        if let Some(reason) = &f.skip {
            out.push_str(&format!("-- {}: SKIPPED — {reason}\n", f.name));
            continue;
        }
        for n in &f.notes {
            out.push_str(&format!("-- {} note: {n}\n", f.name));
        }
        if !f.doc.is_empty() {
            for line in f.doc.lines() {
                let t = line.trim();
                if !t.is_empty() {
                    out.push_str(&format!("-- {}: {t}\n", f.name));
                }
            }
        }

        let py_sig = f
            .params
            .iter()
            .map(|p| format!("{}: {}", p.name, p.hnl))
            .collect::<Vec<_>>()
            .join(", ");
        let arg_names = f
            .params
            .iter()
            .map(|p| p.name.as_str())
            .collect::<Vec<_>>()
            .join(", ");

        out.push_str(&format!("-- {}({}) -> {}\n", f.py_name, py_sig, f.ret));
        // v1 emits an untyped export: `~[n]~>` signatures describe the
        // extern-call convention (array-of-args), not defined arrows —
        // the intended types stay in the `-- name(params) -> ret` comment.
        out.push_str(&format!("export {} := ({}) ~> do {{\n", f.name, arg_names));
        let (pre, post) = dispatcher_parts(surface, f);
        out.push_str("  _ := py_init(null)\n");
        if f.params.is_empty() {
            out.push_str("  _py_args := \"[]\"\n");
        } else {
            out.push_str(&format!("  _py_args := stringify([{}])\n", arg_names));
        }
        out.push_str(&format!(
            "  _ := py_run(\"{}\" + _py_args + \"{}\")\n",
            escape_hnl_string(&pre),
            escape_hnl_string(&post)
        ));
        out.push_str("  parse(get_env(\"HNL_PY_OUT\"))\n");
        out.push_str("}\n\n");
    }

    out
}

/// The Python snippet baked into each exported arrow, split around the
/// arg payload: replay the recorded `sys.path` entries, import the
/// module (by file location when introspection loaded a loose `.py`,
/// else by name), then `json.loads(r"""<payload>""")` — the payload is
/// the hnl-side `stringify([args])` spliced in raw. A raw triple-quoted
/// literal needs no escaping: `\"` stays intact for `json.loads`, and
/// `"""` cannot occur inside JSON text. The result returns through
/// `HNL_PY_OUT` — python's `os.environ` write does `putenv`, which
/// updates the *live* C environ that hnl's `getenv` reads (the reverse
/// direction — hnl `setenv` → python `os.environ` — does NOT work:
/// CPython snapshots environ at `Py_Initialize`).
fn dispatcher_parts(surface: &ImportSurface, f: &ImportFunction) -> (String, String) {
    let paths = surface
        .path_entries
        .iter()
        .map(|p| format!("'{p}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let import_lines = if let Some(file) = &surface.module_file
        && !surface.path_entries.is_empty()
    {
        // A loose .py file: reproduce the file-location import (the
        // recorded sys.path entries still matter for *its* imports).
        format!(
            "import importlib.util\n\
             _spec = importlib.util.spec_from_file_location('{mod}', '{file}')\n\
             _m = importlib.util.module_from_spec(_spec)\n\
             _spec.loader.exec_module(_m)",
            mod = surface.module,
            file = file,
        )
    } else {
        format!("_m = importlib.import_module('{mod}')", mod = surface.module)
    };
    let pre = format!(
        "import json, os, sys, importlib\n\
         for _p in [{paths}]: sys.path.insert(0, _p)\n\
         {import_lines}\n\
         _a = json.loads(r\"\"\"",
        paths = paths,
    );
    let post = format!(
        "\"\"\")\nos.environ['HNL_PY_OUT'] = json.dumps(getattr(_m, '{py}')(*_a))",
        py = f.py_name,
    );
    (pre, post)
}

/// `\` and `"` escaped, newlines folded to `\n` — the script rides
/// inside one hnl string literal.
fn escape_hnl_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}

// ------------------------------------------------------------------
// Registration helpers (called from lib.rs `register`)
// ------------------------------------------------------------------

pub(crate) fn make_import_source(
    _req: &handl_plugin_codegen::CodegenRequest<'_>,
) -> Result<Box<dyn handl_plugin_codegen::AnySource>, String> {
    Ok(erase_source(PythonImportSource))
}

pub(crate) fn make_import_emitter(
    _req: &handl_plugin_codegen::CodegenRequest<'_>,
) -> Result<Box<dyn handl_plugin_codegen::AnyEmitter>, String> {
    Ok(erase_emitter(PythonImportEmitter))
}
