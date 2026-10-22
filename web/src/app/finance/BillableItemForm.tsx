import { useState } from "react";
import { Button } from "../../components/ui/Button";
import { FormField } from "../../components/ui/FormField";
import TextInput from "../../components/ui/TextInput";
import { inputBase } from "../../components/ui/inputStyles";
import {
  formatCents,
  lineAmountCents,
  parsePriceToCents,
  parseQuantityToHundredths,
} from "../../lib/money";

export type BillableItemKind = "CHARGE" | "DISCOUNT";

/** What `createBillableItem`/`updateBillableItem` take as `input`. */
export interface BillableItemFormValues {
  kind: BillableItemKind;
  date: string;
  description: string;
  /** Sent as typed (trimmed) — the API is the one quantity parser. Always
   * `"1"` for a discount. */
  quantity: string;
  /** Blank means none. Always blank for a discount. */
  unit: string;
  /** For a discount, the (positive) amount it takes off. */
  unitPriceCents: number;
  gstFree: boolean;
}

export interface BillableItemFormInitial {
  kind?: BillableItemKind;
  date: string;
  description: string;
  quantity: string;
  unit?: string;
  /** The price as the user would type it, e.g. `"150"` or `"125.50"`. */
  price: string;
  gstFree?: boolean;
}

/**
 * A charge (date, description, quantity, unit, unit price) or a
 * fixed-amount discount (date, description, amount off): the fields shared
 * by the project
 * page's "Add item" form and each row's inline edit form, with a live
 * amount preview. The parent owns the mutation: it gets validated values
 * via `onSubmit`, and resets the form after a successful create by
 * remounting it (a new `key`).
 *
 * The unit price is parsed here (it has to become integer cents); the
 * quantity is only previewed — it's sent as the typed string, and a bad
 * one comes back as the API's own error message.
 */
export function BillableItemForm({
  idPrefix,
  currency,
  initial,
  submitLabel,
  savingLabel,
  isSaving,
  error,
  onSubmit,
  onCancel,
}: {
  /** Makes the field ids unique when several forms are on one page. */
  idPrefix: string;
  currency: string;
  initial: BillableItemFormInitial;
  submitLabel: string;
  savingLabel: string;
  isSaving: boolean;
  error: string | null;
  onSubmit: (values: BillableItemFormValues) => void;
  onCancel?: () => void;
}) {
  const [kind, setKind] = useState<BillableItemKind>(initial.kind ?? "CHARGE");
  const isDiscount = kind === "DISCOUNT";
  const [unit, setUnit] = useState(initial.unit ?? "");
  const [date, setDate] = useState(initial.date);
  const [description, setDescription] = useState(initial.description);
  const [quantity, setQuantity] = useState(initial.quantity);
  const [price, setPrice] = useState(initial.price);
  const [gstFree, setGstFree] = useState(initial.gstFree ?? false);
  const [priceError, setPriceError] = useState<string | null>(null);

  const quantityHundredths = isDiscount
    ? 100
    : parseQuantityToHundredths(quantity);
  const priceCents = parsePriceToCents(price);
  const amount =
    quantityHundredths !== null && priceCents !== null
      ? lineAmountCents(quantityHundredths, priceCents) * (isDiscount ? -1 : 1)
      : null;

  function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (isSaving) return;
    if (priceCents === null || (isDiscount && priceCents === 0)) {
      setPriceError(
        isDiscount
          ? "Enter the amount to take off, above zero, with at most 2 decimal places."
          : "Enter a unit price from 0 to 10,000,000.00, with at most 2 decimal places.",
      );
      return;
    }
    setPriceError(null);
    onSubmit({
      kind,
      date,
      description: description.trim(),
      quantity: isDiscount ? "1" : quantity.trim(),
      unit: isDiscount ? "" : unit.trim(),
      unitPriceCents: priceCents,
      gstFree,
    });
  }

  const id = (field: string) => `${idPrefix}-${field}`;

  return (
    <form onSubmit={handleSubmit} className="flex flex-col gap-4">
      <fieldset className="flex flex-wrap gap-4 text-sm text-ink">
        <legend className="sr-only">Kind of line</legend>
        <label className="flex items-center gap-2">
          <input
            type="radio"
            name={id("kind")}
            checked={!isDiscount}
            onChange={() => setKind("CHARGE")}
          />
          Charge
        </label>
        <label className="flex items-center gap-2">
          <input
            type="radio"
            name={id("kind")}
            checked={isDiscount}
            onChange={() => setKind("DISCOUNT")}
          />
          Discount (a fixed amount off)
        </label>
      </fieldset>
      <div className="sm:w-48">
        <FormField label="Date" htmlFor={id("date")}>
          <TextInput
            id={id("date")}
            type="date"
            value={date}
            onChange={(e) => setDate(e.target.value)}
            required
          />
        </FormField>
      </div>
      <FormField label="Description" htmlFor={id("description")}>
        <textarea
          id={id("description")}
          className={inputBase}
          rows={3}
          value={description}
          onChange={(e) => setDescription(e.target.value)}
          maxLength={2000}
          required
          aria-describedby={id("description-hint")}
        />
        <p id={id("description-hint")} className="text-xs text-ink-muted">
          Lines starting with &quot;* &quot; become bullet points on the
          invoice.
        </p>
      </FormField>
      <div
        className={`grid grid-cols-2 gap-4 sm:items-end ${isDiscount ? "sm:grid-cols-[10rem_1fr]" : "sm:grid-cols-[8rem_6rem_10rem_1fr]"}`}
      >
        {!isDiscount && (
          <>
            <FormField label="Quantity" htmlFor={id("quantity")}>
              <TextInput
                id={id("quantity")}
                inputMode="decimal"
                autoComplete="off"
                value={quantity}
                onChange={(e) => setQuantity(e.target.value)}
                placeholder="e.g. 1.5"
                required
              />
            </FormField>
            <FormField label="Unit" htmlFor={id("unit")}>
              <TextInput
                id={id("unit")}
                autoComplete="off"
                value={unit}
                onChange={(e) => setUnit(e.target.value)}
                maxLength={20}
                placeholder="e.g. hrs"
              />
            </FormField>
          </>
        )}
        <FormField
          label={
            isDiscount ? `Amount off (${currency})` : `Unit price (${currency})`
          }
          htmlFor={id("price")}
          error={priceError}
        >
          <TextInput
            id={id("price")}
            inputMode="decimal"
            autoComplete="off"
            value={price}
            onChange={(e) => setPrice(e.target.value)}
            placeholder="e.g. 150.00"
            required
            aria-invalid={priceError ? true : undefined}
          />
        </FormField>
        <p
          className="col-span-2 text-sm text-ink-muted sm:col-span-1 sm:pb-2"
          aria-live="polite"
        >
          Amount:{" "}
          <span
            className="font-medium text-ink-strong tabular-nums"
            data-testid={id("amount")}
          >
            {amount === null ? "—" : `${formatCents(amount)} ${currency}`}
          </span>
        </p>
      </div>

      <label className="flex items-center gap-2 text-sm text-ink">
        <input
          id={id("gst-free")}
          type="checkbox"
          checked={gstFree}
          onChange={(e) => setGstFree(e.target.checked)}
          className="size-4 rounded-sm border-line text-accent focus:ring-2 focus:ring-accent/25"
        />
        {isDiscount
          ? "GST-free (comes off the GST-free lines)"
          : "GST-free (no GST is charged on this line)"}
      </label>

      {error && (
        <p role="alert" className="text-sm text-red-600 dark:text-red-400">
          {error}
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
