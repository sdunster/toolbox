/**
 * Money helpers for finance. Everything is integer cents / integer
 * hundredths, mirroring `api/src/finance/money.rs` — the server is the
 * source of truth (it parses the quantity string itself and computes every
 * `amountCents`); these exist for input parsing and live previews.
 */

/** Largest unit price, in cents: 10,000,000.00. */
export const MAX_UNIT_PRICE_CENTS = 1_000_000_000;

/** Largest quantity, in hundredths: 1,000,000. */
export const MAX_QUANTITY_HUNDREDTHS = 100_000_000;

const wholeFormatter = new Intl.NumberFormat("en-AU", {
  maximumFractionDigits: 0,
});

/**
 * `123456` → `"1,234.56"`: en-AU grouping, always 2 dp, no currency symbol
 * (the currency code is shown separately, e.g. in a column header). Built
 * from the integer parts rather than `cents / 100`, so it stays exact for
 * every safe integer.
 */
export function formatCents(cents: number): string {
  const sign = cents < 0 ? "-" : "";
  const abs = Math.abs(Math.trunc(cents));
  const whole = Math.floor(abs / 100);
  const frac = abs % 100;
  return `${sign}${wholeFormatter.format(whole)}.${String(frac).padStart(2, "0")}`;
}

/**
 * Parse a user-typed unit price into cents: `"400"`, `"400.5"`,
 * `"1,200.00"`, `"$75"`. Commas are treated as thousands separators and
 * ignored. Returns `null` for anything else — negatives, more than 2 dp,
 * garbage, or over {@link MAX_UNIT_PRICE_CENTS}.
 */
export function parsePriceToCents(input: string): number | null {
  const s = input.trim().replace(/^\$/, "").replace(/,/g, "");
  const match = /^(\d*)(?:\.(\d{1,2}))?$/.exec(s);
  if (!match || (match[1] === "" && match[2] === undefined)) return null;
  const wholeDigits = match[1].replace(/^0+(?=\d)/, "");
  if (wholeDigits.length > 8) return null;
  const whole = wholeDigits === "" ? 0 : Number(wholeDigits);
  const frac = match[2] === undefined ? 0 : Number(match[2].padEnd(2, "0"));
  const cents = whole * 100 + frac;
  return cents <= MAX_UNIT_PRICE_CENTS ? cents : null;
}

/**
 * Cents as a plain editable string, no grouping: `40000` → `"400"`,
 * `12550` → `"125.50"`. What an edit form pre-fills its price field with.
 */
export function centsToInput(cents: number): string {
  const whole = Math.floor(cents / 100);
  const frac = cents % 100;
  return frac === 0
    ? String(whole)
    : `${whole}.${String(frac).padStart(2, "0")}`;
}

/**
 * Mirrors the API's `parse_quantity`: `"2"` → 200, `"1.5"` → 150,
 * `"0.25"` → 25, `".5"` → 50. `null` for blank, garbage, more than 2 dp,
 * zero, negatives, or over 1,000,000.
 */
export function parseQuantityToHundredths(input: string): number | null {
  const s = input.trim();
  const match = /^(\d*)(?:\.(\d{1,2}))?$/.exec(s);
  if (!match || (match[1] === "" && match[2] === undefined)) return null;
  const wholeDigits = match[1].replace(/^0+(?=\d)/, "");
  if (wholeDigits.length > 7) return null;
  const whole = wholeDigits === "" ? 0 : Number(wholeDigits);
  const frac = match[2] === undefined ? 0 : Number(match[2].padEnd(2, "0"));
  const hundredths = whole * 100 + frac;
  if (hundredths <= 0 || hundredths > MAX_QUANTITY_HUNDREDTHS) return null;
  return hundredths;
}

/** Hundredths as the shortest decimal string: 150 → `"1.5"`. */
export function formatQuantity(hundredths: number): string {
  const whole = Math.floor(hundredths / 100);
  const frac = hundredths % 100;
  if (frac === 0) return String(whole);
  if (frac % 10 === 0) return `${whole}.${frac / 10}`;
  return `${whole}.${String(frac).padStart(2, "0")}`;
}

/**
 * round-half-up(quantityHundredths × unitPriceCents / 100), the API's line
 * amount rule. In `BigInt`, since the product can exceed 2^53 at the input
 * bounds even though the result never does.
 */
export function lineAmountCents(
  quantityHundredths: number,
  unitPriceCents: number,
): number {
  const product = BigInt(quantityHundredths) * BigInt(unitPriceCents);
  return Number((product + 50n) / 100n);
}
