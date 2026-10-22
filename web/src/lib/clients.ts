/** A client's editable fields, as the forms hold them. */
export type ClientFormValues = {
  name: string;
  abn: string;
  address: string;
  email: string;
  paymentTermsDays: string;
};

export function initialClientValues(client?: {
  name: string;
  abn?: string | null;
  address?: string | null;
  email?: string | null;
  paymentTermsDays?: number | null;
}): ClientFormValues {
  return {
    name: client?.name ?? "",
    abn: client?.abn ?? "",
    address: client?.address ?? "",
    email: client?.email ?? "",
    paymentTermsDays:
      client?.paymentTermsDays == null ? "" : String(client.paymentTermsDays),
  };
}

/**
 * `ClientInput` from the form, or an error message. Blank optional fields
 * are sent as-is: the API trims them and leaves the attribute out. Blank
 * payment terms mean the business default applies.
 */
export function clientInput(values: ClientFormValues):
  | {
      ok: true;
      input: {
        name: string;
        abn: string;
        address: string;
        email: string;
        paymentTermsDays: number | null;
      };
    }
  | { ok: false; error: string } {
  const terms = values.paymentTermsDays.trim();
  const termsDays = terms === "" ? null : Number.parseInt(terms, 10);
  if (termsDays !== null && (Number.isNaN(termsDays) || termsDays < 0)) {
    return { ok: false, error: "Payment terms must be a number of days." };
  }
  return {
    ok: true,
    input: {
      name: values.name.trim(),
      abn: values.abn.trim(),
      address: values.address.trim(),
      email: values.email.trim(),
      paymentTermsDays: termsDays,
    },
  };
}
