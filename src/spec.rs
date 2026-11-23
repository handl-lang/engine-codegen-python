//! Spec-driven Python `.hnl` generation — the `SpecModule` IR path of
//! the `python` codegen target (ADR-0128 §0.2, PL-004 T-075).
//!
//! A `.yaml`/`.yml`/`.json` spec describes an embedded-CPython shim: the
//! generated `.hnl` module calls `Py_Initialize`/`PyRun_SimpleString`
//! over `ffi.call` and returns each function's output through an
//! env-var side channel. This is the *target's* spec path — it bypasses
//! the C-backend `SdkgenModule` pipeline and rides the
//! [`SpecModule`](handl_plugin_codegen::SpecModule) IR family instead:
//! `SpecModule::load` parses the document, [`PythonSpecEmitter`]
//! renders the `<package>/<package>.hnl` artifact as a `Change`.
//!
//! Everything here is pure — no fs writes (PL-004 side-effect policy).

use handl_plugin_codegen::CodegenError;
use serde::Deserialize;
use std::path::Path;

/// Whether `path` is a spec input (rather than a C header) — kept for
/// callers that pre-flight the input kind (the command layer, T-076).
/// The registry itself routes by target name now; the spec pairing is
/// the `python-spec` target.
pub fn is_python_spec(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("yaml") | Some("yml") | Some("json")
    )
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PythonSpec {
    package: String,
    doc: String,
    library: String,
    python_init: String,
    python_run: String,
    env_get: String,
    env_get_lib: String,
    env_var: String,
    functions: Vec<PythonFunction>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PythonFunction {
    name: String,
    doc: String,
    args: Vec<PythonArg>,
    script: String,
    env: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct PythonArg {
    name: String,
    #[serde(rename = "type")]
    ty: String,
}

impl PythonSpec {
    fn defaults() -> Self {
        Self {
            package: String::new(),
            doc: String::new(),
            library: "libpython3.14.so".to_string(),
            python_init: "Py_Initialize".to_string(),
            python_run: "PyRun_SimpleString".to_string(),
            env_get: "getenv".to_string(),
            env_get_lib: "libc.so.6".to_string(),
            env_var: "HANDL_PY_OUT".to_string(),
            functions: Vec::new(),
        }
    }
}

/// Parse + validate a [`SpecModule`](handl_plugin_codegen::SpecModule)
/// `document` into a [`PythonSpec`]. Pure.
fn spec_from_document(document: &serde_json::Value) -> Result<PythonSpec, CodegenError> {
    let mut spec: PythonSpec = serde_json::from_value(document.clone())
        .map_err(|e| CodegenError::Parse(format!("invalid python spec document: {e}")))?;
    if spec.package.is_empty() {
        return Err(CodegenError::Parse(
            "spec is missing `package` name".to_string(),
        ));
    }
    if spec.functions.is_empty() {
        return Err(CodegenError::Parse("spec has no `functions`".to_string()));
    }
    // Merge any defaults that were not explicitly supplied.
    let defaults = PythonSpec::defaults();
    if spec.library.is_empty() {
        spec.library = defaults.library;
    }
    if spec.python_init.is_empty() {
        spec.python_init = defaults.python_init;
    }
    if spec.python_run.is_empty() {
        spec.python_run = defaults.python_run;
    }
    if spec.env_get.is_empty() {
        spec.env_get = defaults.env_get;
    }
    if spec.env_get_lib.is_empty() {
        spec.env_get_lib = defaults.env_get_lib;
    }
    if spec.env_var.is_empty() {
        spec.env_var = defaults.env_var;
    }
    Ok(spec)
}

/// The `(artifact, content)` pair a spec document materializes to:
/// `<package>/<package>.hnl`. Pure — the emitter wraps it in a
/// `Change::text`.
pub(crate) fn python_hnl_artifact(
    document: &serde_json::Value,
) -> Result<(String, String), CodegenError> {
    let spec = spec_from_document(document)?;
    let artifact = format!("{}/{}.hnl", spec.package, spec.package);
    Ok((artifact, build_python_hnl(&spec)))
}

fn build_python_hnl(spec: &PythonSpec) -> String {
    let mut out = String::new();

    if !spec.doc.is_empty() {
        let lines: Vec<&str> = spec.doc.lines().collect();
        let mut first = true;
        for line in lines {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                if !first {
                    out.push_str("---\n");
                }
                continue;
            }
            if first {
                out.push_str(&format!("--- {} -- {}\n", spec.package, trimmed));
                first = false;
            } else {
                out.push_str(&format!("--- {}\n", trimmed));
            }
        }
    }
    out.push('\n');

    out.push_str("from \"ffi\" import { call }\n\n");

    out.push_str(&format!(
        "py_init := () ~> call(\"{}\", \"{}\", [], [], \"void\", false)\n",
        spec.library, spec.python_init
    ));
    out.push_str(&format!(
        "py_run := (code) ~> call(\"{}\", \"{}\", [code], [\"cstr\"], \"i32\", false)\n",
        spec.library, spec.python_run
    ));
    out.push_str(&format!(
        "get_env := (name) ~> call(\"{}\", \"{}\", [name], [\"cstr\"], \"cstr\", false)\n",
        spec.env_get_lib, spec.env_get
    ));
    out.push('\n');

    for f in &spec.functions {
        if !f.doc.is_empty() {
            for line in f.doc.lines() {
                out.push_str(&format!("-- {}\n", line.trim()));
            }
        }

        let args = f
            .args
            .iter()
            .map(|a| a.name.clone())
            .collect::<Vec<_>>()
            .join(", ");

        let env_var = f.env.as_ref().unwrap_or(&spec.env_var);

        out.push_str(&format!("{} := ({}) ~> do {{\n", f.name, args));
        out.push_str("  _ = py_init();\n");
        out.push_str(&build_code_assignment(&f.script, &f.args));
        out.push_str("  _ = py_run(code);\n");
        out.push_str(&format!("  get_env(\"{}\")\n", env_var));
        out.push_str("}\n\n");
    }

    for f in &spec.functions {
        out.push_str(&format!("export {}\n", f.name));
    }

    out
}

fn build_code_assignment(script: &str, args: &[PythonArg]) -> String {
    if args.is_empty() {
        return format!("  code := \"{}\";\n", escape_hnl_string(script));
    }

    let sep = '\u{001D}'; // group separator, unlikely in Python code
    let mut work = script.to_string();
    for arg in args {
        let placeholder = format!("$${}$$", arg.name);
        work = work.replace(&placeholder, &format!("{sep}{}{sep}", arg.name));
    }

    let mut out = String::new();
    out.push_str("  code := \"");
    for (i, piece) in work.split(sep).enumerate() {
        if i % 2 == 0 {
            out.push_str(&escape_hnl_string(piece));
            if i != work.split(sep).count() - 1 {
                out.push_str("\" + ");
            }
        } else {
            out.push_str(piece);
            out.push_str(" + \"");
        }
    }
    out.push_str("\";\n");
    out
}

fn escape_hnl_string(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\"', "\\\"")
}
