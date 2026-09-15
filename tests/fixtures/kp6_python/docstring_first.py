"""KP-6-python curated snippet: docstring-as-first-statement.

A function body's first statement being the docstring (an
expression_statement wrapping a string) followed by the return is the
canonical python body shape; the curated pair is one clean body and one
with a comment interleaved between docstring and return.
"""


def clean(x: int) -> int:
    """Normalize x."""
    return x * 2


def commented(x: int) -> int:
    """Normalize x, loudly."""
    # comment between docstring and return
    return x * 3
