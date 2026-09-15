"""Order-domain models: plain data plus the invariants that guard it.

Everything here is a dataclass because the service pipeline moves these
objects between processes; anything behavioral lives on the service, not
the model. Two invariants are non-negotiable: totals are integer cents,
and every status transition passes through `Order.advance`.
"""

from dataclasses import dataclass, field
from enum import Enum
from typing import Optional

from utils import money_to_cents, normalize_code


class Status(Enum):
    """The order lifecycle; `CANCELLED` is terminal, `PAID` is not."""

    DRAFT = "draft"
    PAID = "paid"
    SHIPPED = "shipped"
    CANCELLED = "cancelled"


@dataclass
class LineItem:
    """One product line: a code, a quantity, and a unit price in cents."""

    code: str
    quantity: int
    unit_cents: int

    def total_cents(self) -> int:
        """Line total = quantity × unit price (integer cents, no rounding)."""
        if self.quantity < 0:
            raise ValueError("quantity must not be negative")
        # The funnel guarantees unit prices are already cents, so a plain
        # product is exact; there is deliberately no discount math here.
        return self.quantity * self.unit_cents


@dataclass
class Order:
    """An order: identity, status, and the lines that make up the total.

    The store keys orders by `order_id` and serializes `version` on every
    write; optimistic-concurrency checks compare versions before flush.
    """

    order_id: int
    code: str
    status: Status = Status.DRAFT
    lines: list[LineItem] = field(default_factory=list)
    note: Optional[str] = None
    version: int = 1

    def __post_init__(self) -> None:
        # Codes are normalized once, at the boundary, never again after.
        self.code = normalize_code(self.code)

    def total_cents(self) -> int:
        """Sum of line totals; the caller renders currency, not us."""
        return sum(line.total_cents() for line in self.lines)

    def advance(self, to: Status) -> None:
        """Move the order along its lifecycle, refusing illegal jumps."""
        legal = {
            Status.DRAFT: {Status.PAID, Status.CANCELLED},
            Status.PAID: {Status.SHIPPED, Status.CANCELLED},
            # SHIPPED orders are out of our hands: no transition at all.
            Status.SHIPPED: set(),
            Status.CANCELLED: set(),
        }
        if to not in legal[self.status]:
            raise ValueError(f"cannot move {self.status.value} -> {to.value}")
        self.status = to
        self.version += 1

    # Rendering stays string-only: no model method returns a tuple.
    def describe(self) -> str:
        """One human-readable line for logs and receipts."""
        total = self.total_cents()
        if self.status is Status.DRAFT:
            state = "not yet paid"
        elif self.status is Status.PAID:
            # Paid orders always show their capture reference downstream.
            state = "paid, awaiting shipment"
        else:
            state = self.status.value
        return f"order {self.order_id} [{self.code}] {state}: {total / 100:.2f}"


@dataclass
class PaymentAttempt:
    """The record of one try at charging an order's total.

    `declined` attempts are kept (they explain retry pacing); only the
    successful one is promoted into the ledger by the service.
    """

    order_id: int
    amount_cents: int
    declined: bool = False

    @classmethod
    def for_order(cls, order: "Order") -> "PaymentAttempt":
        """Build an attempt for the order's current total."""
        return cls(order_id=order.order_id, amount_cents=order.total_cents())

    def render(self) -> str:
        """Audit-line form: amount travels as cents, display as decimal."""
        kind = "declined" if self.declined else "captured"
        return f"payment {kind} {self.amount_cents / 100:.2f} for {self.order_id}"


def parse_line(raw: str) -> LineItem:
    """Parse a `CODE:QTY:PRICE` import row into a LineItem.

    Import rows are the messiest input the service accepts; anything that
    does not match the three-field shape raises before a model is built.
    """
    code, _, rest = raw.partition(":")
    quantity_text, _, price_text = rest.partition(":")
    item = LineItem(
        code=normalize_code(code),
        quantity=int(quantity_text),
        unit_cents=money_to_cents(price_text),
    )
    # Refuse zero-or-negative quantities at the boundary, not at total time.
    if item.quantity < 1:
        raise ValueError(f"quantity must be positive: {raw!r}")
    return item
