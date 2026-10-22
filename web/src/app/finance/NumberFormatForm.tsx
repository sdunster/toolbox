import { useState } from "react";
import { graphql, useFragment, useMutation } from "react-relay";
import type { NumberFormatForm_instance$key } from "./__generated__/NumberFormatForm_instance.graphql";
import type { NumberFormatFormMutation } from "./__generated__/NumberFormatFormMutation.graphql";
import type { NumberFormatFormNextMutation } from "./__generated__/NumberFormatFormNextMutation.graphql";
import { relayMutationErrorMessage } from "../../lib/relayMutationError";
import { Card } from "../../components/ui/Card";
import { Button } from "../../components/ui/Button";
import { FormField } from "../../components/ui/FormField";
import TextInput from "../../components/ui/TextInput";

const numberFormatFormFragment = graphql`
  fragment NumberFormatForm_instance on Instance {
    id
    financeSettings {
      invoiceNumberPrefix
      creditNoteNumberPrefix
      numberPadding
      nextCreditNoteNumber
    }
  }
`;

/** `"INV-"`, 4, 42 → `"INV-0042"` — the API's `NumberFormat`, for the
 * example only. */
function formatDocumentNumber(
  prefix: string,
  padding: number,
  n: number,
): string {
  return `${prefix}${String(n).padStart(padding, "0")}`;
}

/**
 * How invoice and credit-note numbers print (`setNumberFormat`), and the
 * next credit-note number (`setNextCreditNoteNumber`, forward-only like
 * the invoice one) — owner-or-superuser, alongside `InvoiceNumberingForm`.
 * A format change applies to documents issued from then on: each one's
 * number is frozen as it was printed.
 */
export function NumberFormatForm({
  instance,
}: {
  instance: NumberFormatForm_instance$key;
}) {
  const data = useFragment(numberFormatFormFragment, instance);
  const settings = data.financeSettings;
  const [invoicePrefix, setInvoicePrefix] = useState(
    settings?.invoiceNumberPrefix ?? "",
  );
  const [creditNotePrefix, setCreditNotePrefix] = useState(
    settings?.creditNoteNumberPrefix ?? "CN-",
  );
  const [padding, setPadding] = useState(String(settings?.numberPadding ?? 3));
  const [nextCreditNote, setNextCreditNote] = useState(
    String(settings?.nextCreditNoteNumber ?? 1),
  );
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  const [commitFormat, isSavingFormat] = useMutation<NumberFormatFormMutation>(
    graphql`
      mutation NumberFormatFormMutation(
        $instanceId: ID!
        $invoicePrefix: String!
        $creditNotePrefix: String!
        $padding: Int!
      ) {
        setNumberFormat(
          instanceId: $instanceId
          invoicePrefix: $invoicePrefix
          creditNotePrefix: $creditNotePrefix
          padding: $padding
        ) {
          ...NumberFormatForm_instance
        }
      }
    `,
  );
  const [commitNext, isSavingNext] = useMutation<NumberFormatFormNextMutation>(
    graphql`
      mutation NumberFormatFormNextMutation($instanceId: ID!, $next: Int!) {
        setNextCreditNoteNumber(instanceId: $instanceId, next: $next) {
          ...NumberFormatForm_instance
        }
      }
    `,
  );

  if (!settings) return null;
  const isSaving = isSavingFormat || isSavingNext;
  const paddingValue = Number(padding);
  const paddingOk =
    Number.isInteger(paddingValue) && paddingValue >= 1 && paddingValue <= 8;

  function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (isSaving || !settings) return;
    const next = Number(nextCreditNote);
    if (!paddingOk) {
      setError("Padding must be a whole number from 1 to 8.");
      return;
    }
    if (!Number.isInteger(next) || next < 1) {
      setError(
        "The next credit note number must be a whole number of 1 or more.",
      );
      return;
    }
    setError(null);
    setSaved(false);
    const fail = (err: Error) =>
      setError(relayMutationErrorMessage(err, "Failed to save numbering."));
    const saveNext = () => {
      if (next === settings.nextCreditNoteNumber) {
        setSaved(true);
        return;
      }
      commitNext({
        variables: { instanceId: data.id, next },
        onCompleted: () => setSaved(true),
        onError: fail,
      });
    };
    commitFormat({
      variables: {
        instanceId: data.id,
        invoicePrefix: invoicePrefix.trim(),
        creditNotePrefix: creditNotePrefix.trim(),
        padding: paddingValue,
      },
      onCompleted: saveNext,
      onError: fail,
    });
  }

  const examplePadding = paddingOk ? paddingValue : 3;
  return (
    <Card>
      <h2 className="mb-4 text-sm font-semibold tracking-wide text-ink-muted uppercase">
        Number format
      </h2>
      <form onSubmit={handleSubmit} className="flex flex-col gap-4">
        <div className="grid gap-4 sm:grid-cols-3">
          <FormField label="Invoice prefix" htmlFor="invoice-number-prefix">
            <TextInput
              id="invoice-number-prefix"
              value={invoicePrefix}
              onChange={(e) => setInvoicePrefix(e.target.value)}
              maxLength={12}
              placeholder="None"
            />
          </FormField>
          <FormField
            label="Credit note prefix"
            htmlFor="credit-note-number-prefix"
          >
            <TextInput
              id="credit-note-number-prefix"
              value={creditNotePrefix}
              onChange={(e) => setCreditNotePrefix(e.target.value)}
              maxLength={12}
              required
            />
          </FormField>
          <FormField label="Digits" htmlFor="number-padding">
            <TextInput
              id="number-padding"
              type="number"
              min={1}
              max={8}
              value={padding}
              onChange={(e) => setPadding(e.target.value)}
            />
          </FormField>
        </div>
        <p className="-mt-2 text-xs text-ink-muted">
          Prints as{" "}
          <span className="font-medium text-ink">
            {formatDocumentNumber(invoicePrefix.trim(), examplePadding, 42)}
          </span>{" "}
          and{" "}
          <span className="font-medium text-ink">
            {formatDocumentNumber(creditNotePrefix.trim(), examplePadding, 7)}
          </span>
          . Applies to invoices and credit notes issued from now on.
        </p>
        <FormField
          label="Next credit note number"
          htmlFor="next-credit-note-number"
        >
          <div className="w-32">
            <TextInput
              id="next-credit-note-number"
              type="number"
              min={1}
              step={1}
              inputMode="numeric"
              value={nextCreditNote}
              onChange={(e) => setNextCreditNote(e.target.value)}
            />
          </div>
          <p className="mt-1 text-xs text-ink-muted">
            It can only move forward.
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
          {isSaving ? "Saving…" : "Save numbering"}
        </Button>
      </form>
    </Card>
  );
}
