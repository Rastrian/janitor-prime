"""KP-6-python curated snippet: decorator-above-def adjacency.

Two decorated functions: the first clean (@decorator directly above def),
the second with a comment deliberately interleaved between the decorator
and the def — both parse; only the second differs between strict and
skip adjacency.
"""


@app.route("/clean")
def clean() -> str:
    return "clean"


@cache
# comment between decorator and def
def cached() -> str:
    return "cached"
