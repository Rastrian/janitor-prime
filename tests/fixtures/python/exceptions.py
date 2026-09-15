"""Domain exceptions for the orders service.

Each failure mode gets its own class so callers can catch narrowly; the
hierarchy stays two levels deep on purpose (deep exception trees are a
smell nobody navigates).
"""


class OrderError(Exception):
    """Base class for every order-domain failure."""

    def __init__(self, order_id: int, message: str) -> None:
        super().__init__(message)
        self.order_id = order_id
        self.message = message


class NotFoundError(OrderError):
    """The referenced order does not exist (anymore, or yet)."""


class StaleVersionError(OrderError):
    """The caller's view of the order is older than the store's."""

    def __init__(self, order_id: int, have: int, want: int) -> None:
        # `have` is what the caller sent; `want` is the store's version.
        super().__init__(order_id, f"stale version: have {have}, want {want}")
        self.have = have
        self.want = want


class PaymentDeclinedError(OrderError):
    """The payment provider refused the charge; retrying may still work."""
