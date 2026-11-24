"""Tiny fixture module for the `python-import` tests — stdlib only.

Covers the v1-supported surface (scalars, List(T), defaults, None
return) plus the loud-skip cases (callable, *args/**kwargs, class,
async, dict return, unannotated builtin-style param).
"""

from __future__ import annotations

from collections.abc import Callable


def add(a: int, b: int) -> int:
    return a + b


def greet(name: str, punct: str = "!") -> str:
    return f"hello, {name}{punct}"


def scale(xs: list[float], factor: float = 1.0) -> list[float]:
    return [x * factor for x in xs]


def total(*xs: int) -> int:
    return sum(xs)


def apply_all(f: Callable[[int], int], xs: list[int]) -> list[int]:
    return [f(x) for x in xs]


def describe(**kw: object) -> dict[str, object]:
    return dict(kw)


def lookup(d: dict[str, int], key: str) -> int | None:
    return d.get(key)


def unannotated(x) -> int:
    return x


def kw_only(type: int, data: str = "x") -> str:
    return f"{type}:{data}"


def collide(type: int, type_: int) -> int:
    return type + type_


class Greeter:
    def hello(self, name: str) -> str:
        return f"hi, {name}"


async def fetch_later(x: int) -> int:
    return x
