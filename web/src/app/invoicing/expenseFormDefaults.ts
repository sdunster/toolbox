import type { ExpenseFormInitial } from "./ExpenseForm";

/** A new expense's starting fields: a purchase, GST pre-filled from the
 * amount, on `date` and (optionally) a fixed project. */
export const blankExpense = (
  date: string,
  projectId: string | null,
): ExpenseFormInitial => ({
  projectId,
  date,
  category: "MATERIALS",
  description: "",
  supplier: "",
  amount: "",
  gst: undefined,
  distanceKm: "",
});
