"""KP-6-python curated snippet: if/elif/else sibling chain.

The if_statement's named children are condition, block, elif_clause,
else_clause; the curated pair is the elif directly followed by the else.
One chain clean, one with a comment between the elif and the else.
"""


def sign(x: int) -> str:
    if x < 0:
        return "neg"
    elif x == 0:
        return "zero"
    else:
        return "pos"


def grade(score: int) -> str:
    if score > 90:
        return "a"
    elif score > 50:
        return "b"
    # comment between elif and else
    else:
        return "c"
