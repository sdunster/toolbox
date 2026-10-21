import type { ExpenseFormInitial } from "./ExpenseForm";
import { DEFAULT_BRAND_ID } from "../../lib/brands";

/** A new expense's starting fields: a purchase, GST pre-filled from the
 * amount, on `date` and (optionally) a fixed project and its brand. */
export const blankExpense = (
  date: string,
  projectId: string | null,
  brandId: string = DEFAULT_BRAND_ID,
): ExpenseFormInitial => ({
  projectId,
  brandId,
  date,
  category: "MATERIALS",
  description: "",
  supplier: "",
  amount: "",
  gst: undefined,
  distanceKm: "",
});
