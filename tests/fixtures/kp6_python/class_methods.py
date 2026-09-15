"""KP-6-python curated snippet: method-in-class adjacency.

Two classes, each with two methods: Repo's are adjacent, Cache's are
separated by a comment (a docstring-like banner, exactly what real class
bodies carry between method groups).
"""


class Repo:
    def find(self) -> list[int]:
        return []

    def save(self, item: int) -> None:
        return None


class Cache:
    def get(self, key: str) -> int | None:
        return None

    # comment between methods
    def put(self, key: str, value: int) -> None:
        return None
