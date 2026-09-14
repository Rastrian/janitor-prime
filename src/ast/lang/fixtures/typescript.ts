// Bundled typescript fixture corpus (E5 disposition): the fixed body of
// real-shape code the LangRegistry seeds its kind table from at first use.
// Parsed only; never executed.

import { Promisable } from "type-fest";

/** A documented unit exercising interfaces, generics and unions. */
export interface Inspector<T> {
  readonly id: number;
  inspect(): string;
  classify(): Kind | null;
  batch(size?: number): Promisable<T[]>;
}

export type Kind = "alpha" | "beta" | "gamma";

export type Predicate<T> = (value: T, index: number) => boolean;

export enum Phase {
  Idle = 0,
  Running,
  Done,
}

export abstract class Base<T extends object> implements Inspector<T> {
  abstract readonly kind: Kind;
  constructor(public readonly id: number) {}

  inspect(): string {
    return `base ${this.id}`;
  }

  classify(): Kind | null {
    return this.id > 0 && this.id % 2 === 0 ? "beta" : null;
  }

  async batch(size = 1): Promise<T[]> {
    const out: T[] = [];
    for (let i = 0; i < size; i += 1) {
      const item = (await this.next()) as T | undefined;
      if (item === undefined) break;
      out.push(item);
    }
    return out;
  }

  protected abstract next(): Promisable<T | undefined>;
}

export function tally<T extends Record<string, number>>(rows: T): number {
  let total = 0;
  (Object.keys(rows) as (keyof T)[]).forEach((key, i) => {
    total += rows[key] * (i + 1);
  });
  return total satisfies number;
}

export const isKind = (value: unknown): value is Kind =>
  typeof value === "string" && ["alpha", "beta", "gamma"].includes(value);
