# Agent guide — tool-codegen-python

`handl-plugin-codegen-emitter-python` — the `python` codegen target for
the Handl codegen pipeline (PL-004): a `ctypes` emitter for C-header
`SdkgenModule` IR plus a spec-driven `.hnl` shim over `SpecModule`.
Part of the `tool-codegen-*` family. Rust 2024.

## Layout

| path | role |
| --- | --- |
| `src/lib.rs` | crate docs + public API |
| `src/import.rs` | ctypes emitter (`source: "c"` → `<module>.py`) |
| `src/spec.rs` | spec-driven `.hnl` shim over `SpecModule` |
| `tests/` | `python_import`, `python_import_surface` + `fixtures/` |
| `scripts/`, `examples/`, `docs/` | helper scripts, examples, target doc |
| `xtask/`, `workflows/gate.yaml`, `bupha.toml`, `.cargo/` | vendored repo gate + CI wiring |

## Contract

- Deps resolve from git (`handl-plugin-codegen` → `handl-lang/host`
  `branch = "main"`); `[patch]` tables map engine/kernel/host siblings
  locally.
- Two emit paths: `python` (ctypes over C headers) and `python-spec`
  (`.hnl` shim) — see `docs/index.md`.

## Verification

```bash
cargo xtask gate   # fmt --check → clippy -D warnings → test → doc
```


<!-- myco:agent-awareness -->

## Agent awareness

- Session start / any work discussion: run `myco status --prompt`
  before proposing next steps — know the board first.
- User describes new work → offer to capture it (`myco todo add`,
  `issue new`, `proposal new`). Don't wait to be asked.
- Picking up a task: `myco resume <ID>` is the briefing.
- Wrapping up: `myco handoff <ID>` anything resumable; keep statuses
  honest (claimed → in-progress → blocked/done).

<!-- /myco:agent-awareness -->
