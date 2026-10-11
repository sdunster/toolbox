import { useState } from "react";
import { graphql, useFragment, useMutation } from "react-relay";
import type { InvoiceCreditNotes_invoice$key } from "./__generated__/InvoiceCreditNotes_invoice.graphql";
import type { InvoiceCreditNotesIssueMutation } from "./__generated__/InvoiceCreditNotesIssueMutation.graphql";
import type { InvoiceCreditNotesPdfMutation } from "./__generated__/InvoiceCreditNotesPdfMutation.graphql";
import type { InvoiceCreditNotesSendMutation } from "./__generated__/InvoiceCreditNotesSendMutation.graphql";
import { Button } from "../../components/ui/Button";
import { FormField } from "../../components/ui/FormField";
import TextInput from "../../components/ui/TextInput";
import { relayMutationErrorMessage } from "../../lib/relayMutationError";
import { formatDate, formatTimestamp, localToday } from "../../lib/dates";
import { formatCents, parsePriceToCents } from "../../lib/money";
import { SendDocumentForm } from "./SendDocumentForm";

const invoiceCreditNotesFragment = graphql`
  fragment InvoiceCreditNotes_invoice on Invoice {
    id
    currency
    gstRegistered
    totalCents
    creditedCents
    project {
      clientEmail
    }
    creditNotes {
      id
      displayNumber
      title
      issueDate
      reason
      totalCents
      sentAt
      sentTo
    }
  }
`;

interface LineDraft {
  description: string;
  amount: string;
  gstFree: boolean;
}

const emptyLine = (): LineDraft => ({
  description: "",
  amount: "",
  gstFree: false,
});

/** What the lines credit, with GST — the same rule the server applies:
 * 10% (half-up) of the lines that aren't GST-free, when registered. */
function creditTotal(
  lines: readonly LineDraft[],
  gstRegistered: boolean,
): number | null {
  let subtotal = 0;
  let taxable = 0;
  for (const line of lines) {
    const cents = parsePriceToCents(line.amount);
    if (cents === null || cents <= 0) return null;
    subtotal += cents;
    if (!line.gstFree) taxable += cents;
  }
  const gst = gstRegistered ? Math.round(taxable / 10) : 0;
  return subtotal + gst;
}

function CreditNoteRow({
  note,
  clientEmail,
}: {
  note: {
    readonly id: string;
    readonly displayNumber: string;
    readonly title: string;
    readonly issueDate: string;
    readonly reason: string;
    readonly totalCents: number;
    readonly sentAt: number | null | undefined;
    readonly sentTo: ReadonlyArray<string>;
  };
  clientEmail: string | null;
}) {
  const [error, setError] = useState<string | null>(null);
  const [sending, setSending] = useState(false);
  const [commitPdf, isPreparing] = useMutation<InvoiceCreditNotesPdfMutation>(
    graphql`
      mutation InvoiceCreditNotesPdfMutation($id: ID!) {
        downloadCreditNotePdf(creditNoteId: $id)
      }
    `,
  );
  const [commitSend, isSending] = useMutation<InvoiceCreditNotesSendMutation>(
    graphql`
      mutation InvoiceCreditNotesSendMutation(
        $id: ID!
        $input: SendDocumentInput!
      ) {
        sendCreditNote(creditNoteId: $id, input: $input) {
          id
          sentAt
          sentTo
        }
      }
    `,
  );

  return (
    <li className="flex flex-col gap-2 p-3 text-sm">
      <div className="flex flex-wrap items-baseline justify-between gap-2">
        <div>
          <p className="font-medium text-ink-strong">
            {note.title} {note.displayNumber}{" "}
            <span className="font-normal text-ink-muted">
              {formatDate(note.issueDate)}
            </span>
          </p>
          <p className="text-ink-muted">{note.reason}</p>
          {note.sentAt && (
            <p className="text-xs text-ink-muted">
              Emailed {formatTimestamp(note.sentAt)} to {note.sentTo.join(", ")}
            </p>
          )}
        </div>
        <span className="font-medium tabular-nums">
          −{formatCents(note.totalCents)}
        </span>
      </div>
      <div className="flex flex-wrap gap-2">
        <Button
          variant="secondary"
          disabled={isPreparing}
          onClick={() => {
            setError(null);
            commitPdf({
              variables: { id: note.id },
              onCompleted: (d) =>
                window.location.assign(d.downloadCreditNotePdf),
              onError: (err) =>
                setError(
                  relayMutationErrorMessage(err, "Failed to prepare the PDF."),
                ),
            });
          }}
        >
          {isPreparing ? "Preparing PDF…" : "Download PDF"}
        </Button>
        {!sending && (
          <Button variant="secondary" onClick={() => setSending(true)}>
            Email to client
          </Button>
        )}
      </div>
      {sending && (
        <SendDocumentForm
          idPrefix={`send-${note.id}`}
          clientEmail={clientEmail}
          isSending={isSending}
          error={error}
          submitLabel="Send email"
          onCancel={() => setSending(false)}
          onSubmit={(input) => {
            setError(null);
            commitSend({
              variables: { id: note.id, input },
              onCompleted: () => setSending(false),
              onError: (err) =>
                setError(
                  relayMutationErrorMessage(
                    err,
                    "Failed to send the credit note.",
                  ),
                ),
            });
          }}
        />
      )}
      {error && !sending && (
        <p role="alert" className="text-sm text-red-600 dark:text-red-400">
          {error}
        </p>
      )}
    </li>
  );
}

/**
 * Credit notes against a finalized invoice — the one way to correct it,
 * since a finalized invoice never changes. Lists the ones issued (PDF and
 * email for each) and issues another: the whole invoice (only while
 * nothing has been credited), or specific GST-exclusive amounts. Issuing
 * is irreversible, and the form says so.
 */
export function InvoiceCreditNotes({
  invoice,
}: {
  invoice: InvoiceCreditNotes_invoice$key;
}) {
  const data = useFragment(invoiceCreditNotesFragment, invoice);
  const [open, setOpen] = useState(false);
  const [mode, setMode] = useState<"full" | "lines">(
    data.creditedCents > 0 ? "lines" : "full",
  );
  const [issueDate, setIssueDate] = useState(() => localToday());
  const [reason, setReason] = useState("");
  const [lines, setLines] = useState<LineDraft[]>([emptyLine()]);
  const [error, setError] = useState<string | null>(null);

  const [commit, isIssuing] = useMutation<InvoiceCreditNotesIssueMutation>(
    graphql`
      mutation InvoiceCreditNotesIssueMutation(
        $invoiceId: ID!
        $input: CreditNoteInput!
      ) {
        issueCreditNote(invoiceId: $invoiceId, input: $input) {
          id
          invoice {
            ...InvoiceCreditNotes_invoice
            ...InvoicePaidControl_invoice
            ...InvoiceListRow_invoice
          }
        }
      }
    `,
  );

  const remaining = data.totalCents - data.creditedCents;
  const total =
    mode === "full" ? data.totalCents : creditTotal(lines, data.gstRegistered);

  function updateLine(i: number, patch: Partial<LineDraft>) {
    setLines((prev) =>
      prev.map((line, j) => (j === i ? { ...line, ...patch } : line)),
    );
  }

  function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (isIssuing) return;
    if (mode === "lines" && total === null) {
      setError("Give every line an amount greater than zero.");
      return;
    }
    setError(null);
    commit({
      variables: {
        invoiceId: data.id,
        input: {
          issueDate,
          reason: reason.trim(),
          lines:
            mode === "full"
              ? null
              : lines.map((l) => ({
                  description: l.description.trim(),
                  amountCents: parsePriceToCents(l.amount) ?? 0,
                  gstFree: l.gstFree,
                })),
        },
      },
      onCompleted: () => {
        setOpen(false);
        setReason("");
        setLines([emptyLine()]);
        setMode("lines");
      },
      onError: (err) =>
        setError(
          relayMutationErrorMessage(err, "Failed to issue the credit note."),
        ),
    });
  }

  return (
    <section
      aria-labelledby="credit-notes-heading"
      className="flex flex-col gap-3 rounded-lg border border-line p-4"
    >
      <div className="flex flex-wrap items-center justify-between gap-3">
        <h2
          id="credit-notes-heading"
          className="text-sm font-semibold tracking-wide text-ink-muted uppercase"
        >
          Credit notes
        </h2>
        {!open && remaining > 0 && (
          <Button variant="secondary" onClick={() => setOpen(true)}>
            Issue credit note
          </Button>
        )}
      </div>

      {data.creditNotes.length === 0 ? (
        !open && (
          <p className="text-sm text-ink-muted">
            None. A finalized invoice can&apos;t be edited — issue a credit note
            to correct or cancel it.
          </p>
        )
      ) : (
        <ul className="divide-y divide-line rounded-md border border-line">
          {data.creditNotes.map((note) => (
            <CreditNoteRow
              key={note.id}
              note={note}
              clientEmail={data.project.clientEmail ?? null}
            />
          ))}
        </ul>
      )}

      {open && (
        <form onSubmit={handleSubmit} className="flex flex-col gap-3">
          <div className="flex flex-wrap gap-4">
            <div className="w-48">
              <FormField label="Date" htmlFor="credit-date">
                <TextInput
                  id="credit-date"
                  type="date"
                  value={issueDate}
                  onChange={(e) => setIssueDate(e.target.value)}
                  required
                />
              </FormField>
            </div>
            <div className="min-w-60 flex-1">
              <FormField label="Reason" htmlFor="credit-reason">
                <TextInput
                  id="credit-reason"
                  value={reason}
                  onChange={(e) => setReason(e.target.value)}
                  maxLength={500}
                  required
                  placeholder="e.g. Work not completed"
                />
              </FormField>
            </div>
          </div>
          <fieldset className="flex flex-wrap gap-4 text-sm text-ink">
            <legend className="sr-only">What to credit</legend>
            <label className="flex items-center gap-2">
              <input
                type="radio"
                name="credit-mode"
                checked={mode === "full"}
                disabled={data.creditedCents > 0}
                onChange={() => setMode("full")}
              />
              The whole invoice
            </label>
            <label className="flex items-center gap-2">
              <input
                type="radio"
                name="credit-mode"
                checked={mode === "lines"}
                onChange={() => setMode("lines")}
              />
              Specific amounts
            </label>
          </fieldset>
          {mode === "lines" && (
            <div className="flex flex-col gap-2">
              {lines.map((line, i) => (
                <div key={i} className="flex flex-wrap items-end gap-2">
                  <div className="min-w-48 flex-1">
                    <FormField
                      label="Description"
                      htmlFor={`credit-line-${i}-description`}
                    >
                      <TextInput
                        id={`credit-line-${i}-description`}
                        value={line.description}
                        onChange={(e) =>
                          updateLine(i, { description: e.target.value })
                        }
                        required
                      />
                    </FormField>
                  </div>
                  <div className="w-32">
                    <FormField
                      label="Amount (ex GST)"
                      htmlFor={`credit-line-${i}-amount`}
                    >
                      <TextInput
                        id={`credit-line-${i}-amount`}
                        inputMode="decimal"
                        value={line.amount}
                        onChange={(e) =>
                          updateLine(i, { amount: e.target.value })
                        }
                        required
                      />
                    </FormField>
                  </div>
                  {data.gstRegistered && (
                    <label className="flex items-center gap-2 pb-2 text-sm text-ink">
                      <input
                        type="checkbox"
                        checked={line.gstFree}
                        onChange={(e) =>
                          updateLine(i, { gstFree: e.target.checked })
                        }
                      />
                      GST-free
                    </label>
                  )}
                  {lines.length > 1 && (
                    <Button
                      type="button"
                      variant="ghost"
                      onClick={() =>
                        setLines((prev) => prev.filter((_, j) => j !== i))
                      }
                    >
                      Remove
                    </Button>
                  )}
                </div>
              ))}
              <Button
                type="button"
                variant="ghost"
                className="self-start px-0"
                onClick={() => setLines((prev) => [...prev, emptyLine()])}
              >
                + Add line
              </Button>
            </div>
          )}
          <p className="text-sm text-ink">
            Credits{" "}
            <span className="font-semibold tabular-nums">
              {total === null ? "—" : formatCents(total)} {data.currency}
            </span>{" "}
            {data.gstRegistered && "including GST "}of the{" "}
            {formatCents(remaining)} still uncredited.
          </p>
          <p className="text-sm text-amber-700 dark:text-amber-400">
            A credit note gets the next credit note number and can&apos;t be
            edited or deleted. This cannot be undone.
          </p>
          {error && (
            <p role="alert" className="text-sm text-red-600 dark:text-red-400">
              {error}
            </p>
          )}
          <div className="flex gap-2">
            <Button type="submit" disabled={isIssuing}>
              {isIssuing ? "Issuing…" : "Issue credit note"}
            </Button>
            <Button
              type="button"
              variant="secondary"
              disabled={isIssuing}
              onClick={() => setOpen(false)}
            >
              Cancel
            </Button>
          </div>
        </form>
      )}
    </section>
  );
}
