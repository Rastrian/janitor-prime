/** Product-code funnel: trim, lower, hand back one canonical token. */
export function normalizeCode(code: string): string {
  return code.trim().toLowerCase();
}

export function expandCode(code: string): string {
  return `${code}-x`;
}

// Documented late: the line formatter joins two codes through the funnel.
export function formatLine(first: string, second: string): string {
  const joined = `${normalizeCode(first)}/${normalizeCode(second)}`;
  return joined.replace(/\/+/g, "/");
}
