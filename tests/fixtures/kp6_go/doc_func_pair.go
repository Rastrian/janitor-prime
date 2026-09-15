// KP-6-go curated snippet: doc-comment-above-func adjacency.
//
// Two function pairs at file level: PlainA/PlainB are adjacent, while
// Documented carries the canonical go doc comment above it (the shape
// every exported func in a real file has) which sits between it and
// DocumentedNeighbor as a named sibling. The function-pair idiom: strict
// sees one pair, skip sees both.

package kp6

func PlainA() {}

func PlainB() {}

// Documented carries a doc comment, as real exported funcs do.
func Documented() {}
