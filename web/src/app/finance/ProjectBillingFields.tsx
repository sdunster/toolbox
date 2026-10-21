import { FormField } from "../../components/ui/FormField";
import TextInput from "../../components/ui/TextInput";
import type { ProjectBillingValues } from "../../lib/projectBilling";

/** Client email, payment-terms override and default rate inputs. */
export function ProjectBillingFields({
  idPrefix,
  values,
  onChange,
}: {
  idPrefix: string;
  values: ProjectBillingValues;
  onChange: (values: ProjectBillingValues) => void;
}) {
  return (
    <>
      <FormField label="Client email" htmlFor={`${idPrefix}-client-email`}>
        <TextInput
          id={`${idPrefix}-client-email`}
          type="email"
          value={values.clientEmail}
          onChange={(e) => onChange({ ...values, clientEmail: e.target.value })}
          maxLength={200}
        />
        <p className="mt-1 text-xs text-ink-muted">
          Where invoices are emailed by default.
        </p>
      </FormField>
      <div className="grid gap-4 sm:grid-cols-2">
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
            onChange={(e) =>
              onChange({ ...values, paymentTermsDays: e.target.value })
            }
            placeholder="Business default"
          />
        </FormField>
        <FormField
          label="Default rate (ex GST)"
          htmlFor={`${idPrefix}-default-rate`}
        >
          <TextInput
            id={`${idPrefix}-default-rate`}
            inputMode="decimal"
            value={values.defaultRate}
            onChange={(e) =>
              onChange({ ...values, defaultRate: e.target.value })
            }
            placeholder="e.g. 95.00"
          />
        </FormField>
      </div>
    </>
  );
}
