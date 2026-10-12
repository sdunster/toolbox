import { tw } from "../../lib/tw";
import { formatDate } from "../../lib/dates";

/**
 * The lg+ column templates for a list of invoice rows — the instance-wide
 * page (with a project column) or one project's section (without). Below
 * `lg` a row stacks into a small card instead, same convention as
 * `billableItemStyles.ts`'s `itemGridCols`.
 */
export const invoiceGridCols = {
  withProject: tw`lg:grid-cols-[6rem_6.5rem_minmax(0,1fr)_8rem_8rem]`,
  withoutProject: tw`lg:grid-cols-[6rem_6.5rem_8rem_8rem]`,
};

export const invoiceStatusBadgeBase = tw`inline-flex items-center rounded-full px-2.5 py-0.5 text-xs font-medium whitespace-nowrap`;

/**
 * "Draft" / "Unpaid" / "Paid DD/MM/YYYY" — a draft has no paid status at
 * all, `UNPAID`/`PAID` (the schema's `InvoiceFilterType`, mirrored here by
 * `paidDate`'s presence) both imply finalized.
 */
export function invoiceStatusLabel(
  status: string,
  paidDate: string | null | undefined,
  overdue = false,
): string {
  if (status !== "FINALIZED") return "Draft";
  if (paidDate) return `Paid ${formatDate(paidDate)}`;
  return overdue ? "Overdue" : "Unpaid";
}

export function invoiceStatusBadgeClass(
  status: string,
  paidDate: string | null | undefined,
  overdue = false,
): string {
  if (status !== "FINALIZED") return tw`bg-surface-sunken text-ink-muted`;
  if (paidDate)
    return tw`bg-green-100 text-green-800 dark:bg-green-900/40 dark:text-green-300`;
  return overdue
    ? tw`bg-red-100 text-red-800 dark:bg-red-900/40 dark:text-red-300`
    : tw`bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300`;
}
