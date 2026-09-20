// KP-6-cpp curated snippet: doc-comment-above-function adjacency.
//
// Two function pairs at file level: PlainA/PlainB are adjacent, while
// Documented carries the canonical cpp doc comment above it, which sits
// between it and PlainB as a named sibling. The def idiom: strict sees
// one pair, skip sees both.

int PlainA(int x) {
    return x + 1;
}

int PlainB(int x) {
    return x + 2;
}

// Documented carries a doc comment, as real free functions do.
int Documented(int x) {
    return x + 3;
}
