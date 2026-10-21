import { useState } from "react";
import { graphql, useFragment, useMutation } from "react-relay";
import type { InvoiceNumberingForm_instance$key } from "./__generated__/InvoiceNumberingForm_instance.graphql";
import type { InvoiceNumberingFormMutation } from "./__generated__/InvoiceNumberingFormMutation.graphql";
import { relayMutationErrorMessage } from "../../lib/relayMutationError";
import { Card } from "../../components/ui/Card";
import { Button } from "../../components/ui/Button";
import { FormField } from "../../components/ui/FormField";
import TextInput from "../../components/ui/TextInput";

const invoiceNumberingFormFragment = graphql`
  fragment InvoiceNumberingForm_instance on Instance {
    id
    financeSettings {
      nextInvoiceNumber
    }
  }
`;

/**
 * A finance instance's next invoice number — owner-or-superuser, same
 * posture as `FinanceSettingsForm`, which this sits alongside on both
 * `/app/finance-settings` and the superuser admin instance page.
 * Forward-only server-side (`setNextInvoiceNumber`): a `CONFLICT` here
 * means someone already finalized an invoice (or set it further forward)
 * since this page loaded, so the shown message is the server's own.
 */
export function InvoiceNumberingForm({
  instance,
}: {
  instance: InvoiceNumberingForm_instance$key;
}) {
  const data = useFragment(invoiceNumberingFormFragment, instance);
  const settings = data.financeSettings;
  const [next, setNext] = useState(() =>
    String(settings?.nextInvoiceNumber ?? 1),
  );
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  const [commit, isSaving] = useMutation<InvoiceNumberingFormMutation>(graphql`
    mutation InvoiceNumberingFormMutation($instanceId: ID!, $next: Int!) {
      setNextInvoiceNumber(instanceId: $instanceId, next: $next) {
        id
        financeSettings {
          nextInvoiceNumber
        }
      }
    }
  `);

  if (!settings) return null;

  function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (isSaving) return;
    const parsed = Number(next);
    if (!Number.isInteger(parsed) || parsed < 1) {
      setError("Enter a whole number of 1 or more.");
      return;
    }
    setError(null);
    setSaved(false);
    commit({
      variables: { instanceId: data.id, next: parsed },
      onCompleted: () => setSaved(true),
      onError: (err) =>
        setError(
          relayMutationErrorMessage(err, "Failed to update the next number."),
        ),
    });
  }

  return (
    <Card>
      <h2 className="mb-4 text-sm font-semibold tracking-wide text-ink-muted uppercase">
        Invoice numbering
      </h2>
      <form onSubmit={handleSubmit} className="flex flex-col gap-4">
        <FormField label="Next invoice number" htmlFor="next-invoice-number">
          <div className="w-32">
            <TextInput
              id="next-invoice-number"
              type="number"
              min={1}
              step={1}
              inputMode="numeric"
              value={next}
              onChange={(e) => setNext(e.target.value)}
            />
          </div>
          <p className="mt-1 text-xs text-ink-muted">
            The number the next finalized invoice will get. It can only move
            forward.
          </p>
        </FormField>

        {error && (
          <p role="alert" className="text-sm text-red-600 dark:text-red-400">
            {error}
          </p>
        )}
        {saved && !error && (
          <p className="text-sm text-green-700 dark:text-green-400">Saved.</p>
        )}

        <Button type="submit" disabled={isSaving} className="self-start">
          {isSaving ? "Saving…" : "Update"}
        </Button>
      </form>
    </Card>
  );
}
