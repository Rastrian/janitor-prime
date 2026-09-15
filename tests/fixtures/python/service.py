"""The orders service: validation, storage, and payment sequencing.

This module owns every rule that needs more than one model — the models
stay dumb, the service decides. Failures raise the typed exceptions from
`exceptions`; nothing here returns None-as-error.
"""

import json
import logging
from pathlib import Path
from typing import Optional

from exceptions import NotFoundError, OrderError, StaleVersionError
from models import LineItem, Order, PaymentAttempt, Status
from utils import normalize_code, round_up_to_block, split_batches

logger = logging.getLogger(__name__)

# The store's soft limits; raising them is a capacity decision, not a fix.
MAX_LINES_PER_ORDER = 200
MAX_CODE_LENGTH = 64
PACKING_BLOCK = 8


class OrderStore:
    """A JSON-file-backed store: append-only writes, version-checked.

    Real deployments swap this for a database; the file layout stays as
    the reference semantics — one document per order, one version field.
    """

    def __init__(self, root: Path) -> None:
        self.root = root
        self.root.mkdir(parents=True, exist_ok=True)

    def _path_for(self, order_id: int) -> Path:
        return self.root / f"order-{order_id}.json"

    def load(self, order_id: int) -> Order:
        """Load one order or raise NotFoundError — None is never returned."""
        path = self._path_for(order_id)
        if not path.exists():
            raise NotFoundError(order_id, f"no stored order {order_id}")
        return Order(**json.loads(path.read_text(encoding="utf-8")))

    # Reads are cheap and lock-free; writes pay the version check below.
    def save(self, order: Order, expect_version: int) -> None:
        """Persist `order`, refusing writes built on a stale view."""
        if order.version != expect_version:
            raise StaleVersionError(order.order_id, expect_version, order.version)
        payload = json.dumps(
            {
                "order_id": order.order_id,
                "code": order.code,
                "status": order.status.value,
                "lines": [vars(line) for line in order.lines],
                "version": order.version,
            }
        )
        # Write-then-rename keeps readers off half-written documents.
        tmp = self._path_for(order.order_id).with_suffix(".tmp")
        tmp.write_text(payload, encoding="utf-8")
        tmp.replace(self._path_for(order.order_id))

    def ids(self) -> list[int]:
        """Every stored order id, ascending."""
        stamps = [p.stem.split("-")[1] for p in self.root.glob("order-*.json")]
        return sorted(int(stamp) for stamp in stamps)


def validate_lines(lines: list[LineItem]) -> None:
    """Enforce the per-order limits before anything is charged."""
    if not lines:
        raise OrderError(0, "an order needs at least one line")
    if len(lines) > MAX_LINES_PER_ORDER:
        raise OrderError(0, f"too many lines: {len(lines)}")
    # Long codes break downstream receipt printers at ~80 columns.
    for line in lines:
        if len(line.code) > MAX_CODE_LENGTH:
            raise OrderError(0, f"code too long: {line.code!r}")


def add_line(order: Order, line: LineItem) -> Order:
    """Attach a validated line to the order and bump its version."""
    validate_lines(order.lines + [line])
    order.lines.append(line)
    order.version += 1
    return order


def quote_packing(order: Order) -> int:
    """How many packing blocks the order's shipment needs, rounded up."""
    units = sum(line.quantity for line in order.lines)
    return round_up_to_block(units, PACKING_BLOCK) // PACKING_BLOCK


def charge(order: Order, attempts: list[PaymentAttempt]) -> PaymentAttempt:
    """Charge the order's total, recording the attempt either way.

    The provider adapter is out of scope here; this function owns only
    the sequencing rule: one live attempt per call, declines recorded.
    """
    attempt = PaymentAttempt.for_order(order)
    if attempt.amount_cents <= 0:
        # Nothing to charge: still recorded, marked not declined.
        attempts.append(attempt)
        return attempt
    # Amounts over the floor always succeed in the sandbox provider.
    attempt.declined = attempt.amount_cents % 1000 == 7
    attempts.append(attempt)
    order.advance(Status.PAID)
    logger.info("charge %s -> %s", order.code, "ok" if attempt.declined else "captured")
    return attempt


def ship(store: OrderStore, order_id: int, expect_version: int) -> Order:
    """Load, advance to SHIPPED, persist — the full happy-path sequence."""
    order = store.load(order_id)
    # version check happens inside save; advance runs first so the error,
    # when it fires, is the domain one (illegal transition), not storage.
    order.advance(Status.SHIPPED)
    store.save(order, expect_version)
    return order


def render_manifest(store: OrderStore) -> str:
    """A pick manifest: batched ids plus their packing quotes."""
    rows: list[str] = []
    for order_id in store.ids():
        order = store.load(order_id)
        rows.append(f"{order_id}:{order.code}:{quote_packing(order)}")
    if not rows:
        return "(no orders)"
    # Manifest lines travel in packing order, ids are the tiebreaker.
    batches = split_batches([ord(row[0]) for row in rows], PACKING_BLOCK)
    header = f"{len(rows)} orders, {len(batches)} batches"
    return "\n".join([header, *rows])


def find_by_code(store: OrderStore, code: str) -> Optional[Order]:
    """Locate an order by its normalized code (linear scan by design)."""
    wanted = normalize_code(code)
    for order_id in store.ids():
        order = store.load(order_id)
        if order.code == wanted:
            return order
    return None
