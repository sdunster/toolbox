import { useState } from "react";
import { graphql, useFragment, useMutation } from "react-relay";
import type { InvoicePaidControl_invoice$key } from "./__generated__/InvoicePaidControl_invoice.graphql";
import type { InvoicePaidControlRecordMutation } from "./__generated__/InvoicePaidControlRecordMutation.graphql";
import type { InvoicePaidControlDeleteMutation } from "./__generated__/InvoicePaidControlDeleteMutation.graphql";
import { Button } from "../../components/ui/Button";
import { FormField } from "../../components/ui/FormField";
import TextInput from "../../components/ui/TextInput";
import { relayMutationErrorMessage } from "../../lib/relayMutationError";
import { formatDate, localToday } from "../../lib/dates";
import { centsToInput, formatCents, parsePriceToCents } from "../../lib/money";
import { InvoiceDownloadButton } from "./InvoiceDownloadButton";

const invoicePaidControlFragment = graphql`
  fragment InvoicePaidControl_invoice on Invoice {
    id
    currency
    dueDate
    paidDate
    overdue
    daysOverdue
    totalCents
    paidCents
    creditedCents
    balanceCents
    payments {
      id
      date
      amountCents
      note
    }
  }
`;

/**
 * A finalized invoice's money: what's owed and when it's due, the payments
 * recorded against it (each removable), and a form to record another — its
 * amount defaults to the whole balance, so "paid in full" is one click.
 * The invoice is PAID once payments and credit notes cover its total.
 * Also renders the "Download PDF" button.
 */
export function InvoicePaidControl({
  invoice,
}: {
  invoice: InvoicePaidControl_invoice$key;
}) {
  const data = useFragment(invoicePaidControlFragment, invoice);
  const [date, setDate] = useState(() => localToday());
  const [amount, setAmount] = useState<string | null>(null);
  const [note, setNote] = useState("");
  const [error, setError] = useState<string | null>(null);

  const [commitRecord, isRecording] =
    useMutation<InvoicePaidControlRecordMutation>(graphql`
      mutation InvoicePaidControlRecordMutation(
        $invoiceId: ID!
        $input: RecordPaymentInput!
      ) {
        recordInvoicePayment(invoiceId: $invoiceId, input: $input) {
          ...InvoicePaidControl_invoice
          ...InvoiceListRow_invoice
        }
      }
    `);
  const [commitDelete, isDeleting] =
    useMutation<InvoicePaidControlDeleteMutation>(graphql`
      mutation InvoicePaidControlDeleteMutation(
        $invoiceId: ID!
        $paymentId: ID!
      ) {
        deleteInvoicePayment(invoiceId: $invoiceId, paymentId: $paymentId) {
          ...InvoicePaidControl_invoice
          ...InvoiceListRow_invoice
        }
      }
    `);

  const amountText = amount ?? centsToInput(Math.max(data.balanceCents, 0));

  function record(e: React.FormEvent) {
    e.preventDefault();
    const cents = parsePriceToCents(amountText);
    if (cents === null || cents <= 0) {
      setError("Enter the amount received, like 1250 or 1250.50.");
      return;
    }
    setError(null);
    commitRecord({
      variables: {
        invoiceId: data.id,
        input: { date, amountCents: cents, note: note.trim() || null },
      },
      onCompleted: () => {
        setAmount(null);
        setNote("");
      },
      onError: (err) =>
        setError(relayMutationErrorMessage(err, "Failed to record payment.")),
    });
  }

  function remove(paymentId: string) {
    setError(null);
    commitDelete({
      variables: { invoiceId: data.id, paymentId },
      onError: (err) =>
        setError(relayMutationErrorMessage(err, "Failed to remove payment.")),
    });
  }

  const busy = isRecording || isDeleting;

  return (
    <div className="flex flex-col gap-4 rounded-lg border border-line p-4">
      <div className="flex flex-wrap items-baseline justify-between gap-3">
        <div>
          {data.paidDate ? (
            <p className="text-sm font-medium text-green-700 dark:text-green-400">
              Paid — settled on {formatDate(data.paidDate)}
            </p>
          ) : data.overdue ? (
            <p className="text-sm font-medium text-red-700 dark:text-red-400">
              Overdue by {data.daysOverdue} day
              {data.daysOverdue === 1 ? "" : "s"}
              {data.dueDate && ` (due ${formatDate(data.dueDate)})`}
            </p>
          ) : (
            <p className="text-sm text-ink">
              {data.dueDate ? `Due ${formatDate(data.dueDate)}` : "Unpaid"}
            </p>
          )}
        </div>
        <dl className="flex flex-wrap gap-x-5 gap-y-1 text-sm tabular-nums">
          <div className="flex gap-1">
            <dt className="text-ink-muted">Total</dt>
            <dd>{formatCents(data.totalCents)}</dd>
          </div>
          {data.creditedCents > 0 && (
            <div className="flex gap-1">
              <dt className="text-ink-muted">Credited</dt>
              <dd>{formatCents(data.creditedCents)}</dd>
            </div>
          )}
          <div className="flex gap-1">
            <dt className="text-ink-muted">Paid</dt>
            <dd>{formatCents(data.paidCents)}</dd>
          </div>
          <div className="flex gap-1 font-semibold">
            <dt>Balance</dt>
            <dd>
              {formatCents(data.balanceCents)} {data.currency}
            </dd>
          </div>
        </dl>
      </div>

      {data.payments.length > 0 && (
        <div>
          <h3 className="mb-2 text-xs font-semibold tracking-wide text-ink-muted uppercase">
            Payments
          </h3>
          <ul className="divide-y divide-line rounded-md border border-line">
            {data.payments.map((p) => (
              <li
                key={p.id}
                className="flex flex-wrap items-center justify-between gap-2 px-3 py-2 text-sm"
              >
                <span className="tabular-nums">{formatDate(p.date)}</span>
                <span className="min-w-0 flex-1 truncate text-ink-muted">
                  {p.note}
                </span>
                <span className="font-medium tabular-nums">
                  {formatCents(p.amountCents)}
                </span>
                <Button
                  variant="ghost"
                  disabled={busy}
                  onClick={() => remove(p.id)}
                  aria-label={`Remove payment of ${formatCents(p.amountCents)} on ${formatDate(p.date)}`}
                >
                  Remove
                </Button>
              </li>
            ))}
          </ul>
        </div>
      )}

      {data.balanceCents > 0 && (
        <form onSubmit={record} className="flex flex-wrap items-end gap-3">
          <div className="w-40">
            <FormField label="Payment date" htmlFor="payment-date">
              <TextInput
                id="payment-date"
                type="date"
                value={date}
                onChange={(e) => setDate(e.target.value)}
                required
              />
            </FormField>
          </div>
          <div className="w-36">
            <FormField
              label={`Amount (${data.currency})`}
              htmlFor="payment-amount"
            >
              <TextInput
                id="payment-amount"
                inputMode="decimal"
                value={amountText}
                onChange={(e) => setAmount(e.target.value)}
                required
              />
            </FormField>
          </div>
          <div className="min-w-40 flex-1">
            <FormField label="Note" htmlFor="payment-note">
              <TextInput
                id="payment-note"
                value={note}
                onChange={(e) => setNote(e.target.value)}
                maxLength={200}
                placeholder="e.g. EFT ref 1234"
              />
            </FormField>
          </div>
          <Button type="submit" disabled={busy}>
            {isRecording ? "Saving…" : "Record payment"}
          </Button>
        </form>
      )}

      {error && (
        <p role="alert" className="text-sm text-red-600 dark:text-red-400">
          {error}
        </p>
      )}
      <InvoiceDownloadButton invoiceId={data.id} />
    </div>
  );
}
