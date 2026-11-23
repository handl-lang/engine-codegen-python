# engine-codegen-python

`handl-plugin-codegen-emitter-python` — the `python` codegen target for
the Handl codegen pipeline (PL-004): a `ctypes` emitter for C-header
`SdkgenModule` IR plus a spec-driven `.hnl` shim over `SpecModule`.
Part of the `engine-codegen-*` family.

## Development

```bash
cargo xtask gate        # fmt --check → clippy -D warnings → test → doc
```

`cargo xtask` wraps the repo's `xtask/` crate; `ci.sh` is the gate entry.
Deps resolve from git, so no sibling checkouts are needed; a plain clone
test-runs as-is.

## Docs

- [docs/index.md](docs/index.md) — the `python`/`python-spec`
  targets: the ctypes emitter (`source: "c"` → `<module>.py`) and the
  spec-driven embedded-CPython `.hnl` input path.
- Framework: [`host/docs/sdkgen/`](https://github.com/handl-lang/host/blob/main/docs/sdkgen/index.md).

## License

MIT.
