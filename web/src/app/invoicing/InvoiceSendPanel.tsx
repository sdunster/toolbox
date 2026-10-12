import { useState } from "react";
import { graphql, useFragment, useMutation } from "react-relay";
import type { InvoiceSendPanel_invoice$key } from "./__generated__/InvoiceSendPanel_invoice.graphql";
import type { InvoiceSendPanelMutation } from "./__generated__/InvoiceSendPanelMutation.graphql";
import { Button } from "../../components/ui/Button";
import { relayMutationErrorMessage } from "../../lib/relayMutationError";
import { SendDocumentForm } from "./SendDocumentForm";
import { formatTimestamp } from "../../lib/dates";

const invoiceSendPanelFragment = graphql`
  fragment InvoiceSendPanel_invoice on Invoice {
    id
    sentAt
    sentTo
    project {
      clientEmail
    }
  }
`;

/**
 * "Email to client" for a finalized invoice (`sendInvoice`): the PDF goes
 * out attached, from the business name, with replies to the business
 * email. Shows when it was last sent; sending again is how a reminder
 * goes out.
 */
export function InvoiceSendPanel({
  invoice,
}: {
  invoice: InvoiceSendPanel_invoice$key;
}) {
  const data = useFragment(invoiceSendPanelFragment, invoice);
  const [open, setOpen] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [sent, setSent] = useState(false);
  const [commit, isSending] = useMutation<InvoiceSendPanelMutation>(graphql`
    mutation InvoiceSendPanelMutation(
      $invoiceId: ID!
      $input: SendDocumentInput!
    ) {
      sendInvoice(invoiceId: $invoiceId, input: $input) {
        ...InvoiceSendPanel_invoice
      }
    }
  `);

  return (
    <div className="flex flex-col gap-3 rounded-lg border border-line p-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="text-sm text-ink">
          {data.sentAt
            ? `Emailed ${formatTimestamp(data.sentAt)} to ${data.sentTo.join(", ")}`
            : "Not emailed yet."}
        </p>
        {!open && (
          <Button
            variant="secondary"
            onClick={() => {
              setOpen(true);
              setSent(false);
            }}
          >
            {data.sentAt ? "Send again" : "Email to client"}
          </Button>
        )}
      </div>
      {sent && !open && (
        <p className="text-sm text-green-700 dark:text-green-400">Sent.</p>
      )}
      {open && (
        <SendDocumentForm
          idPrefix="send-invoice"
          clientEmail={data.project.clientEmail ?? null}
          isSending={isSending}
          error={error}
          submitLabel="Send email"
          onCancel={() => setOpen(false)}
          onSubmit={(input) => {
            setError(null);
            commit({
              variables: { invoiceId: data.id, input },
              onCompleted: () => {
                setOpen(false);
                setSent(true);
              },
              onError: (err) =>
                setError(
                  relayMutationErrorMessage(err, "Failed to send the invoice."),
                ),
            });
          }}
        />
      )}
    </div>
  );
}
