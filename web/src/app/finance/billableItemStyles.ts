import { tw } from "../../lib/tw";

/** `BillableItemStatusType` badge colors and labels. */
export const itemStatusBadge: Record<string, string> = {
  UNBILLED: tw`bg-amber-100 text-amber-800 dark:bg-amber-900/40 dark:text-amber-300`,
  DRAFT: tw`bg-surface-sunken text-ink-muted`,
  INVOICED: tw`bg-green-100 text-green-800 dark:bg-green-900/40 dark:text-green-300`,
};

export const itemStatusLabel: Record<string, string> = {
  UNBILLED: "Unbilled",
  DRAFT: "Draft",
  INVOICED: "Invoiced",
};

export const itemStatusBadgeBase = tw`inline-flex items-center rounded-full px-2.5 py-0.5 text-xs font-medium whitespace-nowrap`;

/**
 * The lg+ column templates, shared by the header row and every item row so
 * the columns line up. Below `lg` a row stacks into a small card instead
 * (see `BillableItemRow`).
 *
 * - `withProject`: the instance-wide list — date, project, description,
 *   qty, unit price, amount, status.
 * - `withActions`: a project's own list — no project column, an actions
 *   column instead.
 */
export const itemGridCols = {
  withProject: tw`lg:grid-cols-[6rem_minmax(0,11rem)_minmax(0,1fr)_4.5rem_7rem_8rem_5.5rem]`,
  withActions: tw`lg:grid-cols-[6rem_minmax(0,1fr)_4.5rem_7rem_8rem_5.5rem_10rem]`,
};
