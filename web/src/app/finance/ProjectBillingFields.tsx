import { FormField } from "../../components/ui/FormField";
import TextInput from "../../components/ui/TextInput";
import type { ProjectBillingValues } from "../../lib/projectBilling";

/** The project's default rate input. */
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
    <FormField
      label="Default rate (ex GST)"
      htmlFor={`${idPrefix}-default-rate`}
    >
      <TextInput
        id={`${idPrefix}-default-rate`}
        inputMode="decimal"
        value={values.defaultRate}
        onChange={(e) => onChange({ ...values, defaultRate: e.target.value })}
        placeholder="e.g. 95.00"
      />
      <p className="mt-1 text-xs text-ink-muted">
        What a new billable item costs when it names no price.
      </p>
    </FormField>
  );
}
