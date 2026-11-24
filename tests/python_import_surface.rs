//! `python-import` — JSON→IR mapping, the `.hnl` emitter, and an
//! end-to-end source→emitter pass over a real (tiny, stdlib-only)
//! fixture module.
//!
//! Tests `#![allow]` liberally per repo convention.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use handl_plugin_codegen::{ArtifactRef, Change, EmitContext, Emitter, Source, SpecModule};
use handl_plugin_codegen_emitter_python::{
    PythonImportEmitter, PythonImportSource, build_hnl, map_ann, surface_from_document,
};
use serde_json::json;
use std::path::Path;

fn fixture_json() -> serde_json::Value {
    // Mirrors scripts/hnl-introspect.py's output shape for
    // tests/fixtures/toy_surface.py — kept inline so the mapping tests
    // never depend on a Python interpreter.
    json!({
        "tool": "hnl-introspect",
        "module": "toy_surface",
        "module_file": "/abs/tests/fixtures/toy_surface.py",
        "path_entries": ["/abs/tests/fixtures"],
        "libpython": "libpython3.14.so",
        "python": "3.14.8",
        "functions": [
            {
                "name": "add", "py_name": "add", "doc": "",
                "skip": null,
                "params": [
                    {"name": "a", "ann": "int", "kind": "positional", "has_default": false},
                    {"name": "b", "ann": "int", "kind": "positional", "has_default": false}
                ],
                "ret": "int"
            },
            {
                "name": "greet", "py_name": "greet", "doc": "",
                "skip": null,
                "params": [
                    {"name": "name", "ann": "string", "kind": "positional", "has_default": false},
                    {"name": "punct", "ann": "string", "kind": "positional", "has_default": true}
                ],
                "ret": "string"
            },
            {
                "name": "scale", "py_name": "scale", "doc": "",
                "skip": null,
                "params": [
                    {"name": "xs", "ann": "list[number]", "kind": "positional", "has_default": false},
                    {"name": "factor", "ann": "number", "kind": "positional", "has_default": true}
                ],
                "ret": "list[number]"
            },
            {
                "name": "total", "py_name": "total", "doc": "",
                "skip": null,
                "params": [
                    {"name": "xs", "ann": "int", "kind": "var_positional", "has_default": false}
                ],
                "ret": "int"
            },
            {
                "name": "apply_all", "py_name": "apply_all", "doc": "",
                "skip": null,
                "params": [
                    {"name": "f", "ann": "callable", "kind": "positional", "has_default": false},
                    {"name": "xs", "ann": "list[int]", "kind": "positional", "has_default": false}
                ],
                "ret": "list[int]"
            },
            {
                "name": "describe", "py_name": "describe", "doc": "",
                "skip": null,
                "params": [
                    {"name": "kw", "ann": "class:object", "kind": "var_keyword", "has_default": false}
                ],
                "ret": "dict"
            },
            {
                "name": "lookup", "py_name": "lookup", "doc": "",
                "skip": null,
                "params": [
                    {"name": "d", "ann": "dict", "kind": "positional", "has_default": false},
                    {"name": "key", "ann": "string", "kind": "positional", "has_default": false}
                ],
                "ret": "optional:int"
            },
            {
                "name": "unannotated", "py_name": "unannotated", "doc": "",
                "skip": null,
                "params": [
                    {"name": "x", "ann": "unannotated", "kind": "positional", "has_default": false}
                ],
                "ret": "int"
            },
            {
                "name": "Greeter", "py_name": "Greeter", "doc": "",
                "skip": "class surface — v1 covers module-level functions only",
                "params": [], "ret": "unannotated"
            },
            {
                "name": "fetch_later", "py_name": "fetch_later", "doc": "",
                "skip": "async function — needs an await/event-loop protocol",
                "params": [], "ret": "int"
            }
        ]
    })
}

// ---------------------------------------------------------------
// map_ann — the canonical-string → hnl-type table
// ---------------------------------------------------------------

#[test]
fn map_ann_scalars_and_lists() {
    assert_eq!(map_ann("int").unwrap(), "Int");
    assert_eq!(map_ann("number").unwrap(), "Number");
    assert_eq!(map_ann("string").unwrap(), "String");
    assert_eq!(map_ann("bool").unwrap(), "Bool");
    assert_eq!(map_ann("none").unwrap(), "Value");
    assert_eq!(map_ann("list[int]").unwrap(), "List(Int)");
    assert_eq!(map_ann("list[list[string]]").unwrap(), "List(List(String))");
}

#[test]
fn map_ann_rejects_loudly() {
    for ann in [
        "dict",
        "tuple",
        "set",
        "callable",
        "union",
        "bytes",
        "unannotated",
        "optional:int",
        "class:Foo",
        "str:int",
        "unknown:<class 'X'>",
        "list[callable]",
        "list[dict]",
    ] {
        assert!(map_ann(ann).is_err(), "{ann} must not map");
    }
}

// ---------------------------------------------------------------
// surface_from_document — JSON → classified IR
// ---------------------------------------------------------------

#[test]
fn surface_classifies_supported_and_skipped() {
    let surface = surface_from_document(&fixture_json()).unwrap();
    assert_eq!(surface.module, "toy_surface");
    assert_eq!(surface.libpython, "libpython3.14.so");
    assert_eq!(surface.path_entries, vec!["/abs/tests/fixtures"]);
    assert_eq!(surface.functions.len(), 10);

    let by_name = |n: &str| surface.functions.iter().find(|f| f.name == n).unwrap();

    let add = by_name("add");
    assert!(add.skip.is_none());
    assert_eq!(add.ret, "Int");
    assert_eq!(add.params.len(), 2);
    assert_eq!(add.params[0].hnl, "Int");

    let greet = by_name("greet");
    assert!(greet.skip.is_none());
    assert!(greet.notes.iter().any(|n| n.contains("default")));

    let scale = by_name("scale");
    assert!(scale.skip.is_none());
    assert_eq!(scale.params[0].hnl, "List(Number)");
    assert_eq!(scale.ret, "List(Number)");

    // Whole-function skips, each with a reason.
    for (name, needle) in [
        ("total", "variadic"),
        ("apply_all", "callable"),
        ("describe", "variadic"),
        ("lookup", "dict"),
        ("unannotated", "unannotated"),
        ("Greeter", "class surface"),
        ("fetch_later", "async"),
    ] {
        let f = by_name(name);
        let reason = f
            .skip
            .as_ref()
            .unwrap_or_else(|| panic!("{name} must skip"));
        assert!(reason.contains(needle), "{name}: {reason} missing {needle}");
    }
}

#[test]
fn surface_rejects_non_introspect_documents() {
    let err = surface_from_document(&json!({"package": "x"})).unwrap_err();
    assert!(err.to_string().contains("functions"));
}

// ---------------------------------------------------------------
// build_hnl — the emitted artifact
// ---------------------------------------------------------------

#[test]
fn hnl_output_has_exports_and_loud_skips() {
    let surface = surface_from_document(&fixture_json()).unwrap();
    let hnl = build_hnl(&surface);

    // Supported functions export with typed signatures.
    assert!(hnl.contains("export add := (a, b) ~> do {"));
    assert!(hnl.contains("export scale := (xs, factor) ~> do {"));
    assert!(hnl.contains("export greet := (name, punct) ~> do {"));

    // ffi plumbing + JSON channel.
    assert!(hnl.contains("from \"ffi\" import { call }"));
    assert!(hnl.contains("from \"json\" import { stringify, parse }"));
    assert!(hnl.contains("PYLIB := \"libpython3.14.so\""));
    assert!(hnl.contains("PyRun_SimpleString"));
    assert!(hnl.contains("HNL_PY_IN"));
    assert!(hnl.contains("HNL_PY_OUT"));
    assert!(hnl.contains("importlib.util.spec_from_file_location('toy_surface'"));
    assert!(hnl.contains("sys.path.insert(0, _p)"));

    // Every skipped symbol is a comment — never an export.
    for name in [
        "total",
        "apply_all",
        "describe",
        "lookup",
        "unannotated",
        "Greeter",
        "fetch_later",
    ] {
        assert!(hnl.contains(&format!("-- {name}: SKIPPED")), "{name}");
        assert!(
            !hnl.contains(&format!("export {name}:")),
            "{name} exported!"
        );
    }
    assert!(hnl.contains("callable cannot cross the JSON channel"));
    assert!(hnl.contains("no hnl signature type"));

    // Default-param note survives into the artifact.
    assert!(hnl.contains("greet note:"));
}

#[test]
fn emitter_materializes_one_change() {
    let doc = fixture_json();
    let module = SpecModule {
        format: "python-import".into(),
        name: "toy_surface".into(),
        document: doc,
        source: None,
        diagnostics: Vec::new(),
    };
    let changes = PythonImportEmitter
        .materialize(&module, &EmitContext::new())
        .unwrap();
    assert_eq!(changes.len(), 1);
    let Change::Put {
        artifact, content, ..
    } = &changes[0]
    else {
        panic!("expected Put")
    };
    assert_eq!(
        artifact,
        &handl_plugin_codegen::ArtifactRef::new("toy_surface/toy_surface.hnl")
    );
    let text = String::from_utf8_lossy(content);
    assert!(text.contains("export add := (a, b) ~> do {"));
}

// ---------------------------------------------------------------
// Source + Emitter end-to-end over the real fixture module
// (skips cleanly when no python3 is on PATH)
// ---------------------------------------------------------------

fn python3() -> Option<&'static str> {
    for cand in ["python3", "python"] {
        let ok = std::process::Command::new(cand)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if ok {
            return Some(cand);
        }
    }
    None
}

#[test]
fn source_and_emitter_end_to_end() {
    if python3().is_none() {
        eprintln!("no python interpreter — skipping e2e");
        return;
    }
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/toy_surface.py");
    let ctx = EmitContext::new().with_spec_path(&fixture);
    let module = PythonImportSource.parse(&ctx).expect("introspect fixture");

    assert_eq!(module.format, "python-import");
    assert_eq!(module.name, "toy_surface");
    // Introspection-level skips (Greeter class, fetch_later async)
    // must surface as diagnostics.
    assert!(
        module
            .diagnostics
            .iter()
            .any(|d| d.contains("Greeter") && d.contains("class surface")),
        "diagnostics: {:?}",
        module.diagnostics
    );
    assert!(module.diagnostics.iter().any(|d| d.contains("fetch_later")));

    let changes = PythonImportEmitter
        .materialize(&module, &ctx)
        .expect("materialize");
    let Change::Put {
        artifact, content, ..
    } = &changes[0]
    else {
        panic!("expected Put")
    };
    assert_eq!(artifact, &ArtifactRef::new("toy_surface/toy_surface.hnl"));
    let text = String::from_utf8_lossy(content);

    // The real introspection must agree with the fixture JSON: the
    // supported set exports, everything else is a SKIPPED comment.
    assert!(text.contains("export add := (a, b) ~> do {"));
    assert!(text.contains("export scale := (xs, factor) ~> do {"));
    for name in [
        "total",
        "apply_all",
        "describe",
        "lookup",
        "unannotated",
        "Greeter",
        "fetch_later",
    ] {
        assert!(text.contains(&format!("-- {name}: SKIPPED")), "{name}");
    }
}

#[test]
fn source_reports_missing_target_loudly() {
    let ctx = EmitContext::new().with_spec_path("/definitely/not/a/real/module_or_file_9f3a.py");
    // The path does not exist → treated as a dotted module name →
    // import fails inside the interpreter → Parse error with stderr.
    let err = PythonImportSource.parse(&ctx).unwrap_err();
    assert!(err.to_string().contains("python-import"));
}
