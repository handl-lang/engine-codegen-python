#!/usr/bin/env python3
"""hnl-introspect — Python-side surface introspector for `hnl codegen
python-import`.

Introspection must run inside Python (inspect/typing), so the Rust
`python-import` source shells out to this script and parses its JSON on
stdout. Stdlib only; no third-party imports.

Usage:
    python3 hnl-introspect.py <target>            # JSON surface -> stdout

`<target>` resolves as:
  * an existing ``*.py`` file  -> imported by file location (stem is the
    module name; its parent dir goes on ``sys.path`` so sibling imports
    resolve);
  * an existing directory with ``__init__.py`` -> imported as a package
    (parent dir on ``sys.path``);
  * anything else -> treated as a dotted module name imported with the
    current working directory on ``sys.path``.

Output schema (one JSON document):

    {
      "tool": "hnl-introspect",
      "module": "<name>",
      "module_file": "<abs path or null>",
      "path_entries": ["<abs dirs prepended to sys.path>"],
      "libpython": "<sysconfig LDLIBRARY or null>",
      "python": "<X.Y.Z>",
      "functions": [
        {"name": "add", "py_name": "add", "doc": "...", "skip": null,
         "params": [{"name": "a", "ann": "int", "kind": "positional",
                     "has_default": false}],
         "ret": "int"}
      ]
    }

``ann``/``ret`` are *canonical annotation strings* — the Rust emitter
owns the canonical-string -> hnl-type mapping and the v1 skip policy, so
everything this script cannot prove is a plain string the emitter can
reject loudly:

    int | number | string | bool | none | bytes
    list[<inner>]          tuple | dict | set   (container spellings)
    callable | class:<Name> | union | optional:<inner>
    unannotated | str:<raw> | unknown:<repr>

A record's ``skip`` is a human reason (async, class surface, no
signature, ...); ``null`` means the Rust side should classify it.
"""

from __future__ import annotations

import importlib
import importlib.util
import inspect
import json
import os
import sys
import sysconfig
import types
import typing
from pathlib import Path

SCALARS = {
    int: "int",
    float: "number",
    str: "string",
    bool: "bool",
    bytes: "bytes",
    type(None): "none",
}

_PARAM_KINDS = {
    inspect.Parameter.POSITIONAL_ONLY: "positional",
    inspect.Parameter.POSITIONAL_OR_KEYWORD: "positional",
    inspect.Parameter.KEYWORD_ONLY: "keyword_only",
    inspect.Parameter.VAR_POSITIONAL: "var_positional",
    inspect.Parameter.VAR_KEYWORD: "var_keyword",
}


def canon(ann) -> str:
    """Canonical annotation string — see module docstring for the
    vocabulary. Anything unrecognized degrades to a `str:`/`unknown:`
    spelling the emitter refuses rather than guesses at."""
    if ann is inspect.Parameter.empty:
        return "unannotated"
    if ann is None:
        return "none"
    if isinstance(ann, str):
        # Unresolved stringified annotation (`from __future__ import
        # annotations` fallback path) — the raw text, prefixed so the
        # emitter cannot confuse it with a canonical name.
        return f"str:{ann}"
    if ann in SCALARS:
        return SCALARS[ann]

    origin = typing.get_origin(ann)

    if origin in (typing.Union, types.UnionType) or _is_union(ann):
        args = typing.get_args(ann)
        non_none = [a for a in args if a is not type(None)]
        if len(args) == 2 and len(non_none) == 1:
            return f"optional:{canon(non_none[0])}"
        return "union"

    if origin is list or ann is list:
        args = typing.get_args(ann)
        inner = canon(args[0]) if args else "unannotated"
        return f"list[{inner}]"
    if origin is tuple:
        return "tuple"
    if origin is set or origin is frozenset or ann in (set, frozenset):
        return "set"
    if origin is dict or ann is dict:
        return "dict"

    if origin is not None and (
        origin is collections_callable() or ann is collections_callable()
    ):
        return "callable"
    if ann is collections_callable() or (
        inspect.isclass(ann) and issubclass(ann, collections_callable())
    ):
        return "callable"

    if inspect.isclass(ann):
        return f"class:{ann.__name__}"

    return f"unknown:{ann!r}"[:120]


def collections_callable():
    import collections.abc

    return collections.abc.Callable


def _is_union(ann) -> bool:
    return typing.get_origin(ann) is types.UnionType


def resolve_hints(fn) -> dict:
    """`get_type_hints` with a per-name eval fallback — stringified
    annotations that fail wholesale resolution (missing forward refs)
    still resolve name-by-name against the function's globals."""
    try:
        return typing.get_type_hints(fn)
    except Exception:
        hints = {}
        glb = getattr(fn, "__globals__", {}) or {}
        for key, val in (getattr(fn, "__annotations__", None) or {}).items():
            if isinstance(val, str):
                try:
                    val = eval(val, glb)  # noqa: S307 — introspection of the target's own annotations
                except Exception:
                    pass
            hints[key] = val
        return hints


def load_target(target: str):
    """Resolve `<target>` to (module, modname, module_file,
    path_entries). Raises RuntimeError with a human message."""
    path_entries: list[str] = []
    p = Path(target)
    if p.is_file() and p.suffix == ".py":
        modname = p.stem
        parent = str(p.resolve().parent)
        if parent not in sys.path:
            sys.path.insert(0, parent)
            path_entries.append(parent)
        spec = importlib.util.spec_from_file_location(modname, p)
        if spec is None or spec.loader is None:
            raise RuntimeError(f"cannot build import spec for {p}")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module, modname, str(p.resolve()), path_entries

    if p.is_dir():
        init = p / "__init__.py"
        if init.is_file():
            modname = p.name
            parent = str(p.resolve().parent)
            if parent not in sys.path:
                sys.path.insert(0, parent)
                path_entries.append(parent)
            module = importlib.import_module(modname)
            return module, modname, str(init.resolve()), path_entries
        # A plain directory: treat it as a sys.path root and import
        # nothing — the caller should have named a module inside it.
        raise RuntimeError(
            f"{target} is a directory without __init__.py — pass a "
            "*.py file, a package dir, or a dotted module name"
        )

    # Dotted module name — cwd on sys.path (recorded so the generated
    # .hnl can reproduce the lookup at runtime).
    cwd = os.getcwd()
    if cwd not in sys.path:
        sys.path.insert(0, cwd)
        path_entries.append(cwd)
    module = importlib.import_module(target)
    modname = getattr(module, "__name__", target)
    module_file = getattr(module, "__file__", None)
    return module, modname, module_file, path_entries


def analyze(mod, modname: str) -> list[dict]:
    """One record per public callable/class member. `skip` is a reason
    string or None (Rust classifies the rest)."""
    records = []
    for name, obj in sorted(vars(mod).items()):
        if name.startswith("_"):
            continue
        if inspect.isclass(obj):
            # Imported classes (re-exports like `collections.abc.
            # Callable`) are not this module's surface — only report
            # classes defined here.
            if getattr(obj, "__module__", None) not in (
                None,
                getattr(mod, "__name__", modname),
            ):
                continue
            # v1 is module-level functions only — the class gets one
            # loud record, not one per method.
            records.append(
                {
                    "name": name,
                    "py_name": name,
                    "doc": inspect.getdoc(obj) or "",
                    "skip": "class surface — v1 covers module-level functions only",
                    "params": [],
                    "ret": "unannotated",
                }
            )
            continue
        if not (inspect.isfunction(obj) or inspect.isbuiltin(obj)):
            continue  # constants / imported data — not callable surface
        # Re-exported callables (`from x import f`) are not this
        # module's own surface — v1 stays conservative and only
        # reports names defined in (or builtins exposed by) the target.
        if getattr(obj, "__module__", None) not in (
            None,
            getattr(mod, "__name__", modname),
            "builtins",
        ):
            continue

        rec = {
            "name": name,
            "py_name": name,
            "doc": inspect.getdoc(obj) or "",
            "skip": None,
            "params": [],
            "ret": "unannotated",
        }

        if inspect.iscoroutinefunction(obj):
            rec["skip"] = "async function — needs an await/event-loop protocol"
            records.append(rec)
            continue

        try:
            sig = inspect.signature(obj)
        except (TypeError, ValueError) as e:
            rec["skip"] = f"no introspectable signature ({e}) — builtin/C extension?"
            records.append(rec)
            continue

        hints = resolve_hints(obj)
        for pname, param in sig.parameters.items():
            rec["params"].append(
                {
                    "name": pname,
                    "ann": canon(hints.get(pname, param.annotation)),
                    "kind": _PARAM_KINDS.get(param.kind, "positional"),
                    "has_default": param.default is not param.empty,
                }
            )
        rec["ret"] = canon(hints.get("return", sig.return_annotation))
        records.append(rec)
    return records


def main(argv: list[str]) -> int:
    if len(argv) != 1:
        print("usage: hnl-introspect.py <module-or-path>", file=sys.stderr)
        return 2
    try:
        module, modname, module_file, path_entries = load_target(argv[0])
    except Exception as e:
        print(f"hnl-introspect: {e}", file=sys.stderr)
        return 1

    doc = {
        "tool": "hnl-introspect",
        "module": modname,
        "module_file": module_file,
        "path_entries": path_entries,
        "libpython": sysconfig.get_config_var("LDLIBRARY"),
        "python": ".".join(str(v) for v in sys.version_info[:3]),
        "functions": analyze(module, modname),
    }
    json.dump(doc, sys.stdout, indent=2)
    sys.stdout.write("\n")
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
