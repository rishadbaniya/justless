"""Valid Python that an edit may write: decorators, match, PEP 695 and f-strings."""
from __future__ import annotations

import asyncio
from dataclasses import dataclass, field


@dataclass(frozen=True, slots=True)
class Point[T]:
    x: T
    y: T
    tags: list[str] = field(default_factory=list)

    @property
    def norm(self) -> float:
        return (self.x**2 + self.y**2) ** 0.5


type Pair[K, V] = tuple[K, V]


def first[T](items: list[T], /, *, default: T | None = None) -> T | None:
    if (n := len(items)) > 0:
        return items[0]
    return default


def describe(command: object) -> str:
    match command:
        case {"action": "move", "to": (x, y)} if x > 0:
            return f"move {x=} {y:>{4}}"
        case [first, *rest]:
            return f"{first!r} and {len(rest)} more"
        case Point(x=0, y=0) | None:
            return "origin"
        case _:
            return f"{command['name'] if isinstance(command, dict) else command}"


def nested(data: dict[str, str]) -> str:
    return f"{data["key"]} {f'{data.get("k", "")}'}"


async def gather(urls: list[str]) -> list[bytes]:
    async with asyncio.timeout(5):
        results = [await fetch(url) async for url in stream(urls)]
    try:
        pass
    except* ValueError as group:
        print(*group.exceptions, sep="\n")
    with (
        open("a") as a,
        open("b") as b,
    ):
        print(a, b)
    return results


lambda_default = lambda x=1, *args, **kwargs: (x, args, kwargs)
matrix = [[i * j for j in range(3)] for i in range(3)]
print(matrix[0], end="")

if __name__ == "__main__":
    asyncio.run(gather([]))
