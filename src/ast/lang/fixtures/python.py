# Bundled python fixture corpus (E5 disposition): the fixed body of
# real-shape code the LangRegistry seeds its kind table from at first use.
# Parsed only; never executed.

from dataclasses import dataclass, field
from typing import Optional


@dataclass
class Widget:
    """A documented unit exercising classes, fields and methods."""

    id: int = 0
    tags: list[str] = field(default_factory=list)

    def classify(self) -> Optional[str]:
        if self.id % 2 == 0 and self.id > 0:
            return "even"
        elif self.id < 0:
            return None
        else:
            return "odd"

    def tally(self, rows: dict[int, int]) -> int:
        acc = 0
        for key, value in rows.items():
            acc += value * (key + 1)
        while acc > 64:
            acc //= 2
        return acc


def comprehend(items: list[int]) -> tuple[int, ...]:
    squares = [n * n for n in items if n != 0]
    lookup = {n: f"#{n}" for n in squares}
    total = sum(squares)
    return total, len(lookup)


def scoped(total: int):
    def scale(factor: int = 2) -> int:
        return total * factor

    yield scale()
    try:
        with open("/dev/null", encoding="utf-8") as handle:
            text = handle.read()
    except (OSError, ValueError) as err:
        print(f"failed: {err!r}")
    else:
        assert text == ""
    finally:
        pass


class Runnable(Widget):
    async def run(self, steps: int) -> None:
        for step in range(steps):
            await self.classify()

    lambda_placeholder = lambda x: x + 1


BANNER: str = "widgetry"
print(BANNER, comprehend([1, 2, 3]), Runnable(id=4).classify())
