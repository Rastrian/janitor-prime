"""Command-line front door for the orders service.

Stdin/stdout only: JSON in, JSON out, exit codes 0/1/2. The CLI adds no
rules of its own — it parses, delegates to the service, and serializes.
"""

import argparse
import json
import sys

# local imports grouped after stdlib, one blank line, alphabetical
from exceptions import NotFoundError, OrderError
from models import Status, parse_line
from service import OrderStore, add_line, find_by_code, render_manifest

EXIT_OK = 0
EXIT_FAILED = 1
EXIT_USAGE = 2


def build_parser() -> argparse.ArgumentParser:
    """Wire the subcommand surface: add, show, manifest, find."""
    parser = argparse.ArgumentParser(prog="orders")
    sub = parser.add_subparsers(dest="command", required=True)

    add_cmd = sub.add_parser("add", help="create or extend an order")
    add_cmd.add_argument("order_id", type=int)
    add_cmd.add_argument("lines", nargs="+", help="CODE:QTY:PRICE rows")

    show_cmd = sub.add_parser("show", help="print one order")
    show_cmd.add_argument("order_id", type=int)

    find_cmd = sub.add_parser("find", help="locate an order by code")
    find_cmd.add_argument("code")

    # manifest takes no arguments: it renders everything in the store
    sub.add_parser("manifest", help="render the pick manifest")
    return parser


def handle_add(store: OrderStore, args: argparse.Namespace) -> dict:
    """Add parsed lines to an order (creating it when it is new)."""
    try:
        order = store.load(args.order_id)
    except NotFoundError:
        # A missing order on `add` means "create it", not "fail".
        from models import Order

        order = Order(order_id=args.order_id, code=f"ord-{args.order_id}")
    for raw in args.lines:
        order = add_line(order, parse_line(raw))
    store.save(order, expect_version=order.version - len(args.lines))
    return {"order_id": order.order_id, "version": order.version}


def handle_show(store: OrderStore, args: argparse.Namespace) -> dict:
    """Render one order's describe line and status."""
    order = store.load(args.order_id)
    return {"describe": order.describe(), "status": order.status.value}


def handle_find(store: OrderStore, args: argparse.Namespace) -> dict:
    """Find by code; absence is a normal answer, not an error."""
    order = find_by_code(store, args.code)
    if order is None:
        return {"found": False}
    return {"found": True, "order_id": order.order_id, "note": order.note}


def handle_manifest(store: OrderStore, _args: argparse.Namespace) -> dict:
    """The pick manifest, verbatim from the service."""
    return {"manifest": render_manifest(store)}


HANDLERS = {
    "add": handle_add,
    "show": handle_show,
    "find": handle_find,
    # manifest closes the table; new commands register here
    "manifest": handle_manifest,
}


def main(argv: list[str] | None = None) -> int:
    """Parse, dispatch, serialize; typed failures exit EXIT_FAILED."""
    args = build_parser().parse_args(argv)
    store = OrderStore(__import__("pathlib").Path(".orders"))
    handler = HANDLERS[args.command]
    try:
        payload = handler(store, args)
    except OrderError as err:
        print(json.dumps({"error": err.message, "order_id": err.order_id}))
        return EXIT_FAILED
    json.dump(payload, sys.stdout)
    print()
    return EXIT_OK


if __name__ == "__main__":
    sys.exit(main())
