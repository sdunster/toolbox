import { centsToInput, parsePriceToCents } from "./money";

/** The project field that drives billing, as the forms hold it. */
export type ProjectBillingValues = {
  defaultRate: string;
};

export function initialBillingValues(project?: {
  defaultUnitPriceCents?: number | null;
}): ProjectBillingValues {
  return {
    defaultRate:
      project?.defaultUnitPriceCents == null
        ? ""
        : centsToInput(project.defaultUnitPriceCents),
  };
}

/**
 * The billing field's input value, or an error message. Blank means "not
 * set": new items need a price.
 */
export function billingInput(
  values: ProjectBillingValues,
):
  | { ok: true; input: { defaultUnitPriceCents: number | null } }
  | { ok: false; error: string } {
  const rate = values.defaultRate.trim();
  const rateCents = rate === "" ? null : parsePriceToCents(rate);
  if (rate !== "" && rateCents === null) {
    return { ok: false, error: "Enter the default rate like 95 or 95.50." };
  }
  return { ok: true, input: { defaultUnitPriceCents: rateCents } };
}
