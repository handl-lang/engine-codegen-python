# tool-sdkgen-python

`handl-plugin-sdkgen-emitter-python` — Python `ctypes` emitter for the
Handl SDK generator. Part of the `tool-sdkgen-*` family: emits Python SDK
bindings from SDK specs.

## Development

```bash
cargo xtask gate        # fmt --check → clippy -D warnings → test → doc
```

`cargo xtask` wraps the repo's `xtask/` crate; `ci.sh` is the gate entry.
Deps resolve from git, so no sibling checkouts are needed; a plain clone
test-runs as-is.

## Docs

- [docs/index.md](docs/index.md) — the `python` target: the ctypes
  emitter (`backend: "c"` → `<module>.py`) and the spec-driven
  embedded-CPython `.hnl` input path.
- Framework: [`host/docs/sdkgen/`](https://github.com/handl-lang/host/blob/main/docs/sdkgen/index.md).

## License

MIT.
