// KP-6-typescript curated snippet: interface-member adjacency, with the
// `;`-vs-newline separator question measured. Both separators are
// ANONYMOUS tokens (`;` or nothing) — neither ever appears in the named
// sibling list, so separator style cannot affect adjacency in either
// mode. A `//` comment between members does: strict sees three adjacent
// pairs over the five members, skip sees four.

interface Rates {
  base: number;
  bonus: number;
  cap: number
  // capped rates are computed last
  floor: number
  ceiling: number
}
