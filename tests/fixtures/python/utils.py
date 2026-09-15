"""Small helpers shared across the orders service.

Pure functions only: no I/O, no globals — everything arrives as an
argument and leaves as a return value, which keeps these trivially
testable and trivially movable.
"""

import math
import re
from typing import Iterable

# ISO-8601 dates and simple money amounts are the two shapes we scrub.
_DATE_RE = re.compile(r"^\d{4}-\d{2}-\d{2}$")
_CENTS_RE = re.compile(r"^-?\d+\.\d{2}$")


def normalize_code(raw: str) -> str:
    """Collapse whitespace and casing so codes compare equal by content.

    Order codes arrive from three channels (API, CSV import, manual
    entry) with wildly different formatting habits; this is the single
    funnel they all pass through.
    """
    folded = raw.casefold().strip()
    # Interior runs of whitespace (tabs, doubled spaces) become one space.
    return " ".join(folded.split())


def money_to_cents(text: str) -> int:
    """Parse a currency string like `-12.34` into integer cents.

    Integer cents are the only money representation the service uses;
    floats never touch money.
    """
    if not _CENTS_RE.match(text):
        raise ValueError(f"not a money amount: {text!r}")
    negative = text.startswith("-")
    whole, _, frac = text.lstrip("-").partition(".")
    cents = int(whole) * 100 + int(frac)
    # Keep the sign on the cents, not on the string, from here on.
    return -cents if negative else cents


def is_valid_date(text: str) -> bool:
    """True when `text` is an ISO date the store is willing to store."""
    if not _DATE_RE.match(text):
        return False
    year, month, day = (int(part) for part in text.split("-"))
    if month < 1 or month > 12:
        return False
    # February owes its days back in leap years (proleptic Gregorian).
    leap = year % 4 == 0 and (year % 100 != 0 or year % 400 == 0)
    days_in_month = (31, 29 if leap else 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31)
    return 1 <= day <= days_in_month[month - 1]


def split_batches(items: Iterable[int], size: int) -> list[list[int]]:
    """Chunk `items` into batches of at most `size` (last one may be short)."""
    if size < 1:
        raise ValueError("batch size must be positive")
    batch: list[int] = []
    out: list[list[int]] = []
    for item in items:
        batch.append(item)
        # Cut a batch exactly when it reaches `size`, never before.
        if len(batch) == size:
            out.append(batch)
            batch = []
    if batch:
        out.append(batch)
    return out


def round_up_to_block(quantity: int, block: int = 8) -> int:
    """Round `quantity` up to the next multiple of `block` (packing rule)."""
    if block < 1:
        raise ValueError("block must be positive")
    return int(math.ceil(quantity / block) * block)
