// KP-6-typescript curated snippet: JSDoc-above-function adjacency.
//
// Two function pairs at program level: plainFirst/plainSecond are
// adjacent, while documented carries the canonical JSDoc block above it
// (the shape every exported function in a real module has), which parks
// between it and its neighbor as a named sibling. Strict sees one pair,
// skip sees both.

function plainFirst(): void {}

function plainSecond(): void {}

/**
 * Documented carries a JSDoc block, as real exported functions do.
 */
function documented(): void {}
