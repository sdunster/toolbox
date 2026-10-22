import { FormField } from "../../components/ui/FormField";
import TextInput from "../../components/ui/TextInput";
import { inputBase } from "../../components/ui/inputStyles";
import type { ClientFormValues } from "../../lib/clients";

/**
 * A client's fields: the "Invoice to" block every invoice prints (name, ABN,
 * address), where invoices are emailed, and its payment terms.
 */
export function ClientFields({
  idPrefix,
  values,
  onChange,
}: {
  idPrefix: string;
  values: ClientFormValues;
  onChange: (values: ClientFormValues) => void;
}) {
  const set = (field: keyof ClientFormValues) => (value: string) =>
    onChange({ ...values, [field]: value });
  return (
    <>
      <FormField label="Client name" htmlFor={`${idPrefix}-name`}>
        <TextInput
          id={`${idPrefix}-name`}
          value={values.name}
          onChange={(e) => set("name")(e.target.value)}
          maxLength={200}
          required
        />
      </FormField>
      <FormField label="ABN" htmlFor={`${idPrefix}-abn`}>
        <TextInput
          id={`${idPrefix}-abn`}
          value={values.abn}
          onChange={(e) => set("abn")(e.target.value)}
          maxLength={200}
        />
      </FormField>
      <FormField label="Address" htmlFor={`${idPrefix}-address`}>
        <textarea
          id={`${idPrefix}-address`}
          className={inputBase}
          rows={3}
          value={values.address}
          onChange={(e) => set("address")(e.target.value)}
          maxLength={2000}
        />
      </FormField>
      <div className="grid gap-4 sm:grid-cols-2">
        <FormField label="Email" htmlFor={`${idPrefix}-email`}>
          <TextInput
            id={`${idPrefix}-email`}
            type="email"
            value={values.email}
            onChange={(e) => set("email")(e.target.value)}
            maxLength={200}
          />
          <p className="mt-1 text-xs text-ink-muted">
            Where invoices are emailed by default.
          </p>
        </FormField>
        <FormField
          label="Payment terms (days)"
          htmlFor={`${idPrefix}-payment-terms`}
        >
          <TextInput
            id={`${idPrefix}-payment-terms`}
            type="number"
            min={0}
            max={365}
            value={values.paymentTermsDays}
            onChange={(e) => set("paymentTermsDays")(e.target.value)}
            placeholder="Business default"
          />
        </FormField>
      </div>
    </>
  );
}
