//! Dev utility: run the `python-import` source+emitter over a Python
//! module/package and print the generated `.hnl` to stdout.
//!
//! ```bash
//! cargo run --example import_dump -- /path/to/module.py
//! cargo run --example import_dump -- some.dotted.module
//! ```
//!
//! Skipped symbols are printed as `-- <name>: SKIPPED` comments in the
//! artifact and as `diagnostic:` lines on stderr — nothing is dropped
//! silently.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use handl_plugin_codegen::{Change, EmitContext, Emitter, Source};
use handl_plugin_codegen_emitter_python::{PythonImportEmitter, PythonImportSource};

fn main() {
    let spec = std::env::args()
        .nth(1)
        .expect("usage: import_dump <module-or-path>");
    let ctx = EmitContext::new().with_spec_path(&spec);
    let module = PythonImportSource
        .parse(&ctx)
        .expect("introspection failed");
    for d in &module.diagnostics {
        eprintln!("diagnostic: {d}");
    }
    let changes = PythonImportEmitter
        .materialize(&module, &ctx)
        .expect("materialize failed");
    for change in changes {
        if let Change::Put { content, .. } = change {
            print!("{}", String::from_utf8_lossy(&content));
        }
    }
}
