/**
 * Expense helpers: category labels, GST and cents-per-km previews, and the
 * financial-year arithmetic behind the 5,000 km running total. The API is
 * the source of truth for every stored value — it parses `distanceKm`,
 * looks up the trip's rate, and computes `amountCents`; the rate table and
 * parsing here mirror `api/src/invoicing/vehicle.rs` for live previews only.
 */

/** `ExpenseCategoryType`, in the order the category picker lists them. */
export const EXPENSE_CATEGORIES = [
  { value: "MATERIALS", label: "Materials & supplies" },
  { value: "SUBCONTRACTORS", label: "Subcontractors" },
  { value: "TOOLS_EQUIPMENT", label: "Tools & equipment" },
  { value: "VEHICLE_FUEL", label: "Vehicle & fuel" },
  { value: "VEHICLE_KM", label: "Vehicle trip (cents per km)" },
  { value: "TRAVEL", label: "Travel" },
  { value: "MEALS_ENTERTAINMENT", label: "Meals & entertainment" },
  { value: "SOFTWARE_SUBSCRIPTIONS", label: "Software & subscriptions" },
  { value: "PHONE_INTERNET", label: "Phone & internet" },
  { value: "OFFICE_SUPPLIES", label: "Office supplies" },
  { value: "PROFESSIONAL_FEES", label: "Professional fees" },
  { value: "INSURANCE", label: "Insurance" },
  { value: "RENT_UTILITIES", label: "Rent & utilities" },
  { value: "ADVERTISING_MARKETING", label: "Advertising & marketing" },
  { value: "BANK_FEES", label: "Bank & merchant fees" },
  { value: "TRAINING", label: "Training & education" },
  { value: "LICENCES_MEMBERSHIPS", label: "Licences & memberships" },
  { value: "POSTAGE_FREIGHT", label: "Postage & freight" },
  { value: "OTHER", label: "Other" },
] as const;

export type ExpenseCategory = (typeof EXPENSE_CATEGORIES)[number]["value"];

export const VEHICLE_KM = "VEHICLE_KM" satisfies ExpenseCategory;

export function categoryLabel(value: string): string {
  return EXPENSE_CATEGORIES.find((c) => c.value === value)?.label ?? value;
}

/** The GST in a GST-inclusive Australian price: a eleventh, rounded to the
 * cent. What the form pre-fills until the user overrides it. */
export function defaultGstCents(amountCents: number): number {
  return Math.round(amountCents / 11);
}

/** The ATO cap the running total is measured against. */
export const ANNUAL_CAP_KM = 5_000;

/** Above this the running total turns amber; above the cap, red. */
export const NEAR_CAP_KM = 4_500;

/**
 * Mirrors `ATO_RATES` in `api/src/invoicing/vehicle.rs`: financial year
 * (by the calendar year it starts in) → cents per km. Preview only.
 */
const ATO_RATES: Record<number, number> = {
  2020: 72,
  2021: 72,
  2022: 78,
  2023: 85,
  2024: 88,
  2025: 88,
  2026: 91,
};

/** The financial year a `YYYY-MM-DD` date falls in, by its starting year:
 * 1 July onward belongs to the year that starts that July. `null` for
 * anything that isn't a date. */
export function financialYearOf(isoDate: string): number | null {
  const match = /^(\d{4})-(\d{2})-\d{2}$/.exec(isoDate);
  if (!match) return null;
  const year = Number(match[1]);
  return Number(match[2]) >= 7 ? year : year - 1;
}

/** `2026` → `"2026–27"`. */
export function formatFinancialYear(startYear: number): string {
  return `${startYear}–${String((startYear + 1) % 100).padStart(2, "0")}`;
}

export function rateForDate(isoDate: string): number | null {
  const fy = financialYearOf(isoDate);
  return fy === null ? null : (ATO_RATES[fy] ?? null);
}

/**
 * Mirrors the API's `parse_distance_km`: `"12"` → 120, `"12.5"` → 125,
 * `".5"` → 5. `null` for blank, garbage, more than 1 dp, zero, or over
 * 5,000 km.
 */
export function parseDistanceToTenths(input: string): number | null {
  const match = /^(\d*)(?:\.(\d))?$/.exec(input.trim());
  if (!match || (match[1] === "" && match[2] === undefined)) return null;
  const wholeDigits = match[1].replace(/^0+(?=\d)/, "");
  if (wholeDigits.length > 4) return null;
  const tenths =
    (wholeDigits === "" ? 0 : Number(wholeDigits)) * 10 +
    (match[2] === undefined ? 0 : Number(match[2]));
  if (tenths <= 0 || tenths > ANNUAL_CAP_KM * 10) return null;
  return tenths;
}

/** round-half-up(tenths × rate / 10) — `trip_amount_cents`. */
export function tripAmountCents(tenths: number, rateCentsPerKm: number) {
  return Math.floor((tenths * rateCentsPerKm + 5) / 10);
}

/** Where a running total stands against the cap — drives its color. */
export function kmCapStatus(totalKm: number): "ok" | "near" | "over" {
  if (totalKm > ANNUAL_CAP_KM) return "over";
  if (totalKm > NEAR_CAP_KM) return "near";
  return "ok";
}
