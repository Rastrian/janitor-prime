"""Orders service package: models, storage, and the CLI front door.

Importing the package imports nothing heavy — submodules are pulled in
explicitly by callers so a `python -c 'import orders'` stays cheap.
"""

from exceptions import NotFoundError, OrderError, StaleVersionError

# re-exports keep call sites stable while the modules move underneath
from models import LineItem, Order, PaymentAttempt, Status
from service import OrderStore

__all__ = [
    "LineItem",
    "Order",
    "OrderError",
    "OrderStore",
    "PaymentAttempt",
    "NotFoundError",
    "StaleVersionError",
    "Status",
]

VERSION = "1.4.2"
