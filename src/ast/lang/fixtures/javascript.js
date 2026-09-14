// Bundled javascript fixture corpus (E5 disposition): the fixed body of
// real-shape code the LangRegistry seeds its kind table from at first use.
// Parsed only; never executed.

import { readFile } from "node:fs/promises";

/** A documented unit exercising classes, fields and methods. */
export class Widget {
  static MAX = 64;

  #id;
  constructor(id = 0) {
    this.#id = id;
    this.tags = [];
  }

  get id() {
    return this.#id;
  }

  classify() {
    if (this.#id > 0 && this.#id % 2 === 0) return "even";
    return this.#id < 0 ? null : "odd";
  }

  async load(path) {
    try {
      const text = await readFile(path, "utf8");
      this.tags = text.split(/\s+/).filter(Boolean);
    } catch (err) {
      console.error(`failed: ${err.message ?? err}`);
    } finally {
      this.done = true;
    }
  }
}

export function* counter(start = 0) {
  let n = start;
  while (true) {
    yield n++;
    if (n > 10) break;
  }
}

export const tally = (rows) => {
  let total = 0;
  for (const [key, value] of Object.entries(rows ?? {})) {
    total += value * (Number(key) + 1);
  }
  return total;
};

const { a, ...rest } = { a: 1, b: 2, c: 3 };
const nested = rows?.deep?.list?.[0] ?? [1, 2, 3].map((n) => n * n);
console.log(a, rest, nested, tally({ 1: 2 }), [...counter()]);
