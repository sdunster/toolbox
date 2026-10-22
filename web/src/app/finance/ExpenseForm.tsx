import { useState } from "react";
import { Button } from "../../components/ui/Button";
import { FormField } from "../../components/ui/FormField";
import TextInput from "../../components/ui/TextInput";
import { inputBase } from "../../components/ui/inputStyles";
import { centsToInput, formatCents, parsePriceToCents } from "../../lib/money";
import {
  EXPENSE_CATEGORIES,
  VEHICLE_KM,
  type ExpenseCategory,
  claimableGstCents,
  defaultGstCents,
  formatFinancialYear,
  gstUsuallyNotClaimable,
  financialYearOf,
  parseDistanceToTenths,
  rateForDate,
  tripAmountCents,
} from "../../lib/expenses";
import { type BrandOption, DEFAULT_BRAND_ID } from "../../lib/brands";
import { BrandSelect } from "./BrandSelect";

/** What the GST report will claim back, as a sentence for the form. */
function claimablePreview(
  gstCents: number | null,
  businessUse: string,
  notClaimable: boolean,
  currency: string,
): string {
  if (gstCents === null) return "No GST to claim.";
  const use = businessUse.trim() === "" ? 100 : Number(businessUse.trim());
  if (!Number.isInteger(use) || use < 1 || use > 100) return "";
  const claimable = claimableGstCents(gstCents, use, notClaimable);
  return notClaimable
    ? "None of the GST is claimed in the GST report."
    : `The GST report claims ${formatCents(claimable)} ${currency} of GST${use < 100 ? ` (${use}% business use)` : ""}.`;
}

/** What `createExpense`/`updateExpense` take as `input` — the fields the
 * chosen category doesn't use are sent as `null`. */
export interface ExpenseFormValues {
  projectId: string | null;
  /** `"default"` for the default brand. */
  brandId: string;
  date: string;
  category: ExpenseCategory;
  description: string | null;
  supplier: string | null;
  amountCents: number | null;
  gstCents: number | null;
  /** `1..=100`; `null` for a trip (wholly business by definition). */
  businessUsePercent: number | null;
  gstNotClaimable: boolean;
  /** Sent as typed (trimmed) — the API is the one distance parser. */
  distanceKm: string | null;
}

/** A project the expense form can pick, with the brand it brings along. */
export interface ExpenseProjectOption {
  readonly id: string;
  readonly name: string;
  readonly brand?: { readonly brandId: string } | null;
}

export interface ExpenseFormInitial {
  projectId: string | null;
  brandId: string;
  date: string;
  category: ExpenseCategory;
  description: string;
  supplier: string;
  /** As the user would type it, e.g. `"110"`; blank for a new expense. */
  amount: string;
  /** `null` with a non-blank `amount` means the expense is GST-free;
   * `undefined` means "not set yet" — pre-fill a eleventh of the amount. */
  gst: string | null | undefined;
  /** As typed; `"100"` (or blank) means wholly for the business. */
  businessUse: string;
  /** `undefined` means "not chosen yet" — follow the category's default. */
  gstNotClaimable: boolean | undefined;
  distanceKm: string;
}

/**
 * Fields for one expense, shared by the "Add expense" forms and each row's
 * inline edit. Two shapes, by category:
 *
 * - **A purchase**: supplier, the GST-inclusive amount paid, and the GST in
 *   it — pre-filled as a eleventh of the amount until the user types their
 *   own, and cleared by "GST-free".
 * - **A vehicle trip** (`VEHICLE_KM`): distance and the trip's business
 *   purpose, with a km × rate preview. The rate shown mirrors the API's
 *   table; the server looks it up again from the date and is the one that
 *   counts.
 *
 * `projects` is the instance's project list for the "Project" picker; omit
 * it on a project's own page, where `initial.projectId` is fixed. `brands`
 * feeds the "Brand" picker (hidden unless the instance has more than one);
 * picking a project switches the brand to that project's.
 */
export function ExpenseForm({
  idPrefix,
  currency,
  initial,
  projects,
  brands,
  submitLabel,
  savingLabel,
  isSaving,
  error,
  onSubmit,
  onCancel,
}: {
  idPrefix: string;
  currency: string;
  initial: ExpenseFormInitial;
  projects?: ReadonlyArray<ExpenseProjectOption>;
  brands?: ReadonlyArray<BrandOption>;
  submitLabel: string;
  savingLabel: string;
  isSaving: boolean;
  error: string | null;
  onSubmit: (values: ExpenseFormValues) => void;
  onCancel?: () => void;
}) {
  const [projectId, setProjectId] = useState(initial.projectId);
  const [brandId, setBrandId] = useState(initial.brandId);
  const [date, setDate] = useState(initial.date);
  const [category, setCategory] = useState<ExpenseCategory>(initial.category);
  const [description, setDescription] = useState(initial.description);
  const [supplier, setSupplier] = useState(initial.supplier);
  const [amount, setAmount] = useState(initial.amount);
  // `null` until the user types their own GST — until then it tracks the
  // amount. An edit form starts with the stored value as if typed.
  const [gstTyped, setGstTyped] = useState<string | null>(
    initial.gst === undefined ? null : (initial.gst ?? ""),
  );
  const [gstFree, setGstFree] = useState(
    initial.gst === null && initial.amount !== "",
  );
  const [businessUse, setBusinessUse] = useState(initial.businessUse);
  // `null` until the user ticks or unticks it — until then it follows the
  // category (entertainment GST usually can't be claimed).
  const [notClaimableChoice, setNotClaimableChoice] = useState<boolean | null>(
    initial.gstNotClaimable ?? null,
  );
  const gstNotClaimable =
    notClaimableChoice ?? gstUsuallyNotClaimable(category);
  const [distanceKm, setDistanceKm] = useState(initial.distanceKm);
  const [fieldError, setFieldError] = useState<string | null>(null);

  const isTrip = category === VEHICLE_KM;
  const amountCents = parsePriceToCents(amount);
  const gstValue =
    gstTyped ??
    (amountCents === null ? "" : centsToInput(defaultGstCents(amountCents)));

  const tenths = parseDistanceToTenths(distanceKm);
  const rate = rateForDate(date);
  const fy = financialYearOf(date);
  const tripAmount =
    tenths !== null && rate !== null ? tripAmountCents(tenths, rate) : null;

  function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (isSaving) return;
    const trimmedDescription = description.trim() || null;
    if (isTrip) {
      setFieldError(null);
      onSubmit({
        projectId,
        brandId,
        date,
        category,
        description: trimmedDescription,
        supplier: null,
        amountCents: null,
        gstCents: null,
        businessUsePercent: null,
        gstNotClaimable: false,
        distanceKm: distanceKm.trim(),
      });
      return;
    }
    if (amountCents === null || amountCents === 0) {
      setFieldError(
        "Enter the amount paid, up to 10,000,000.00, with at most 2 decimal places.",
      );
      return;
    }
    const gstCents = gstFree ? null : parsePriceToCents(gstValue);
    if (!gstFree && (gstCents === null || gstCents > amountCents)) {
      setFieldError(
        "Enter the GST included in the amount (no more than the amount), or tick GST-free.",
      );
      return;
    }
    const use = businessUse.trim() === "" ? 100 : Number(businessUse.trim());
    if (!Number.isInteger(use) || use < 1 || use > 100) {
      setFieldError("Business use must be a whole percentage from 1 to 100.");
      return;
    }
    setFieldError(null);
    onSubmit({
      projectId,
      brandId,
      date,
      category,
      description: trimmedDescription,
      supplier: supplier.trim(),
      amountCents,
      gstCents,
      businessUsePercent: use,
      gstNotClaimable: gstFree ? false : gstNotClaimable,
      distanceKm: null,
    });
  }

  const id = (field: string) => `${idPrefix}-${field}`;

  return (
    <form onSubmit={handleSubmit} className="flex flex-col gap-4">
      <div className="grid gap-4 sm:grid-cols-[10rem_minmax(0,1fr)_minmax(0,1fr)]">
        <FormField label="Date" htmlFor={id("date")}>
          <TextInput
            id={id("date")}
            type="date"
            value={date}
            onChange={(e) => setDate(e.target.value)}
            required
          />
        </FormField>
        <FormField label="Category" htmlFor={id("category")}>
          <select
            id={id("category")}
            className={inputBase}
            value={category}
            onChange={(e) => setCategory(e.target.value as ExpenseCategory)}
          >
            {EXPENSE_CATEGORIES.map((c) => (
              <option key={c.value} value={c.value}>
                {c.label}
              </option>
            ))}
          </select>
        </FormField>
        {projects && (
          <FormField label="Project" htmlFor={id("project")}>
            <select
              id={id("project")}
              className={inputBase}
              value={projectId ?? ""}
              onChange={(e) => {
                const next = e.target.value || null;
                setProjectId(next);
                const project = projects.find((p) => p.id === next);
                if (project) {
                  setBrandId(project.brand?.brandId ?? DEFAULT_BRAND_ID);
                }
              }}
            >
              <option value="">No project</option>
              {projects.map((p) => (
                <option key={p.id} value={p.id}>
                  {p.name}
                </option>
              ))}
            </select>
          </FormField>
        )}
      </div>

      <BrandSelect
        id={id("brand")}
        brands={brands}
        value={brandId}
        current={initial.brandId}
        onChange={setBrandId}
      />

      {isTrip ? (
        <>
          <FormField label="Business purpose" htmlFor={id("description")}>
            <TextInput
              id={id("description")}
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              maxLength={2000}
              placeholder="e.g. Site visit, 12 Example St"
              required
            />
          </FormField>
          <div className="grid grid-cols-2 gap-4 sm:grid-cols-[10rem_1fr] sm:items-end">
            <FormField label="Distance (km)" htmlFor={id("distance")}>
              <TextInput
                id={id("distance")}
                inputMode="decimal"
                autoComplete="off"
                value={distanceKm}
                onChange={(e) => setDistanceKm(e.target.value)}
                placeholder="e.g. 12.5"
                required
              />
            </FormField>
            <p
              className="col-span-2 text-sm text-ink-muted sm:col-span-1 sm:pb-2"
              aria-live="polite"
            >
              {rate === null ? (
                fy === null ? (
                  "Pick a date to see the rate."
                ) : (
                  `No ATO rate for FY ${formatFinancialYear(fy)} yet.`
                )
              ) : (
                <>
                  At {rate}c/km (FY {formatFinancialYear(fy ?? 0)}):{" "}
                  <span
                    className="font-medium text-ink-strong tabular-nums"
                    data-testid={id("trip-amount")}
                  >
                    {tripAmount === null
                      ? "—"
                      : `${formatCents(tripAmount)} ${currency}`}
                  </span>
                </>
              )}
            </p>
          </div>
          <p className="-mt-2 text-xs text-ink-muted">
            The ATO&apos;s cents-per-km rate covers all running costs, so
            don&apos;t also claim fuel for a car you claim this way.
          </p>
        </>
      ) : (
        <>
          <FormField label="Supplier" htmlFor={id("supplier")}>
            <TextInput
              id={id("supplier")}
              value={supplier}
              onChange={(e) => setSupplier(e.target.value)}
              maxLength={200}
              required
            />
          </FormField>
          <div className="grid grid-cols-2 gap-4 sm:grid-cols-[10rem_10rem_1fr] sm:items-end">
            <FormField
              label={`Amount incl. GST (${currency})`}
              htmlFor={id("amount")}
            >
              <TextInput
                id={id("amount")}
                inputMode="decimal"
                autoComplete="off"
                value={amount}
                onChange={(e) => setAmount(e.target.value)}
                placeholder="e.g. 110.00"
                required
              />
            </FormField>
            <FormField label={`GST (${currency})`} htmlFor={id("gst")}>
              <TextInput
                id={id("gst")}
                inputMode="decimal"
                autoComplete="off"
                value={gstFree ? "" : gstValue}
                disabled={gstFree}
                onChange={(e) => setGstTyped(e.target.value)}
              />
            </FormField>
            <label className="col-span-2 flex items-center gap-2 text-sm text-ink sm:col-span-1 sm:pb-2">
              <input
                type="checkbox"
                checked={gstFree}
                onChange={(e) => setGstFree(e.target.checked)}
                className="size-4 rounded-sm border-line text-accent focus:ring-2 focus:ring-accent/25"
              />
              GST-free
            </label>
          </div>
          <div className="grid grid-cols-2 gap-4 sm:grid-cols-[10rem_1fr] sm:items-end">
            <FormField label="Business use (%)" htmlFor={id("business-use")}>
              <TextInput
                id={id("business-use")}
                type="number"
                min={1}
                max={100}
                value={businessUse}
                onChange={(e) => setBusinessUse(e.target.value)}
                placeholder="100"
              />
            </FormField>
            {!gstFree && (
              <label className="col-span-2 flex items-center gap-2 text-sm text-ink sm:col-span-1 sm:pb-2">
                <input
                  type="checkbox"
                  checked={gstNotClaimable}
                  onChange={(e) => setNotClaimableChoice(e.target.checked)}
                  className="size-4 rounded-sm border-line text-accent focus:ring-2 focus:ring-accent/25"
                />
                GST can&apos;t be claimed
              </label>
            )}
          </div>
          <p
            className="-mt-2 text-xs text-ink-muted"
            aria-live="polite"
            data-testid={id("claimable")}
          >
            {claimablePreview(
              gstFree ? null : parsePriceToCents(gstValue),
              businessUse,
              gstNotClaimable,
              currency,
            )}
          </p>
          <FormField label="Notes (optional)" htmlFor={id("description")}>
            <TextInput
              id={id("description")}
              value={description}
              onChange={(e) => setDescription(e.target.value)}
              maxLength={2000}
            />
          </FormField>
        </>
      )}

      {(fieldError ?? error) && (
        <p role="alert" className="text-sm text-red-600 dark:text-red-400">
          {fieldError ?? error}
        </p>
      )}

      <div className="flex gap-2">
        <Button type="submit" disabled={isSaving}>
          {isSaving ? savingLabel : submitLabel}
        </Button>
        {onCancel && (
          <Button variant="secondary" onClick={onCancel} disabled={isSaving}>
            Cancel
          </Button>
        )}
      </div>
    </form>
  );
}
