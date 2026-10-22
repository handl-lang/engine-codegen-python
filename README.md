# sdkgen-python

`handl-plugin-sdkgen-emitter-python` — Python `ctypes` emitter for the Handl SDK generator (ADR-0107).

Sibling-repo deps are git dependencies (`branch = "main"`); `[patch]` tables
resolve them from sibling checkouts.

## Docs

- [docs/index.md](docs/index.md) — the `python` target: the ctypes
  emitter (`backend: "c"` → `<module>.py`) and the spec-driven
  embedded-CPython `.hnl` input path.
- Framework: [`host/docs/sdkgen/`](https://github.com/handl-lang/host/blob/main/docs/sdkgen/index.md).
