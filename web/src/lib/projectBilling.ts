import { centsToInput, parsePriceToCents } from "./money";

/** The three project fields that drive billing, as the forms hold them. */
export type ProjectBillingValues = {
  clientEmail: string;
  paymentTermsDays: string;
  defaultRate: string;
};

export function initialBillingValues(project?: {
  clientEmail?: string | null;
  paymentTermsDays?: number | null;
  defaultUnitPriceCents?: number | null;
}): ProjectBillingValues {
  return {
    clientEmail: project?.clientEmail ?? "",
    paymentTermsDays:
      project?.paymentTermsDays == null ? "" : String(project.paymentTermsDays),
    defaultRate:
      project?.defaultUnitPriceCents == null
        ? ""
        : centsToInput(project.defaultUnitPriceCents),
  };
}

/**
 * The billing fields' input values, or an error message. Blank means
 * "not set": the instance's terms apply, and new items need a price.
 */
export function billingInput(values: ProjectBillingValues):
  | {
      ok: true;
      input: {
        clientEmail: string;
        paymentTermsDays: number | null;
        defaultUnitPriceCents: number | null;
      };
    }
  | { ok: false; error: string } {
  const terms = values.paymentTermsDays.trim();
  const termsDays = terms === "" ? null : Number.parseInt(terms, 10);
  if (termsDays !== null && (Number.isNaN(termsDays) || termsDays < 0)) {
    return { ok: false, error: "Payment terms must be a number of days." };
  }
  const rate = values.defaultRate.trim();
  const rateCents = rate === "" ? null : parsePriceToCents(rate);
  if (rate !== "" && rateCents === null) {
    return { ok: false, error: "Enter the default rate like 95 or 95.50." };
  }
  return {
    ok: true,
    input: {
      clientEmail: values.clientEmail.trim(),
      paymentTermsDays: termsDays,
      defaultUnitPriceCents: rateCents,
    },
  };
}
