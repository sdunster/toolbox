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
  defaultGstCents,
  formatFinancialYear,
  financialYearOf,
  parseDistanceToTenths,
  rateForDate,
  tripAmountCents,
} from "../../lib/expenses";

/** What `createExpense`/`updateExpense` take as `input` — the fields the
 * chosen category doesn't use are sent as `null`. */
export interface ExpenseFormValues {
  projectId: string | null;
  date: string;
  category: ExpenseCategory;
  description: string | null;
  supplier: string | null;
  amountCents: number | null;
  gstCents: number | null;
  /** Sent as typed (trimmed) — the API is the one distance parser. */
  distanceKm: string | null;
}

export interface ExpenseFormInitial {
  projectId: string | null;
  date: string;
  category: ExpenseCategory;
  description: string;
  supplier: string;
  /** As the user would type it, e.g. `"110"`; blank for a new expense. */
  amount: string;
  /** `null` with a non-blank `amount` means the expense is GST-free;
   * `undefined` means "not set yet" — pre-fill a eleventh of the amount. */
  gst: string | null | undefined;
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
 * it on a project's own page, where `initial.projectId` is fixed.
 */
export function ExpenseForm({
  idPrefix,
  currency,
  initial,
  projects,
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
  projects?: ReadonlyArray<{ readonly id: string; readonly name: string }>;
  submitLabel: string;
  savingLabel: string;
  isSaving: boolean;
  error: string | null;
  onSubmit: (values: ExpenseFormValues) => void;
  onCancel?: () => void;
}) {
  const [projectId, setProjectId] = useState(initial.projectId);
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
        date,
        category,
        description: trimmedDescription,
        supplier: null,
        amountCents: null,
        gstCents: null,
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
    setFieldError(null);
    onSubmit({
      projectId,
      date,
      category,
      description: trimmedDescription,
      supplier: supplier.trim(),
      amountCents,
      gstCents,
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
              onChange={(e) => setProjectId(e.target.value || null)}
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
