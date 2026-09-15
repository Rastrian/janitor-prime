import { normalizeCode } from "./format.js";

export interface ReportRow {
  label: string;
  value: string;
}

// one row per raw input, canonicalized through the funnel
export function rowFor(raw: string): ReportRow {
  const canonical = normalizeCode(raw);
  return { label: canonical, value: canonical.length.toString() };
}

export function rowsFor(raws: string[]): ReportRow[] {
  return raws.map((raw) => rowFor(raw));
}
