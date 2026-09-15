// KP-6-typescript curated snippet: arrow-function-as-argument chains.
//
// Two calls whose argument lists end in a pair of arrow callbacks: the
// first clean, the second with a /* formatter */ note between the two
// arrows (legal anywhere whitespace is). Strict sees one arrow pair,
// skip sees both.

transform(values, (v) => v.trim(), (v) => v.toLowerCase());

render(values, (v) => v.padStart(2, "0"), /* formatter */ (v) => v.slice(1));
