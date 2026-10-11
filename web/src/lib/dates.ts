/**
 * Calendar-date helpers for invoicing's `YYYY-MM-DD` strings. These are
 * dates, not instants — never round-tripped through `Date` parsing, which
 * would read `"2026-08-19"` as UTC midnight and shift it a day west of UTC.
 */

const ISO_DATE = /^(\d{4})-(\d{2})-(\d{2})$/;

/** Today in the browser's own timezone, as `YYYY-MM-DD`. */
export function localToday(now: Date = new Date()): string {
  const y = now.getFullYear();
  const m = String(now.getMonth() + 1).padStart(2, "0");
  const d = String(now.getDate()).padStart(2, "0");
  return `${y}-${m}-${d}`;
}

/** `"2026-08-19"` → `"19/08/2026"`, the invoice's date format. Anything
 * that isn't `YYYY-MM-DD` comes back unchanged. */
export function formatDate(isoDate: string): string {
  const match = ISO_DATE.exec(isoDate);
  if (!match) return isoDate;
  return `${match[3]}/${match[2]}/${match[1]}`;
}

/** `isoDate` plus `days`, as `YYYY-MM-DD` — calendar arithmetic done in UTC
 * so no timezone can shift it. Anything that isn't `YYYY-MM-DD` comes back
 * unchanged. */
export function addDays(isoDate: string, days: number): string {
  const match = ISO_DATE.exec(isoDate);
  if (!match) return isoDate;
  const t = Date.UTC(Number(match[1]), Number(match[2]) - 1, Number(match[3]));
  const d = new Date(t + days * 86_400_000);
  const y = d.getUTCFullYear();
  const m = String(d.getUTCMonth() + 1).padStart(2, "0");
  const day = String(d.getUTCDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

/** Unix seconds → the browser's local date and time. */
export function formatTimestamp(seconds: number): string {
  return new Date(seconds * 1000).toLocaleString();
}
