import { useState } from "react";
import { graphql, useFragment, useMutation } from "react-relay";
import type { InvoiceEinvoice_invoice$key } from "./__generated__/InvoiceEinvoice_invoice.graphql";
import type { InvoiceEinvoiceDownloadMutation } from "./__generated__/InvoiceEinvoiceDownloadMutation.graphql";
import { Button } from "../../components/ui/Button";
import { relayMutationErrorMessage } from "../../lib/relayMutationError";

const invoiceEinvoiceFragment = graphql`
  fragment InvoiceEinvoice_invoice on Invoice {
    id
    status
    einvoiceProblems
  }
`;

/**
 * An invoice's Australian e-invoice (Peppol PINT A-NZ UBL XML). On a draft,
 * says whether it will be issuable as one — and if not, why, while the
 * project and invoicing settings can still be fixed: a finalized invoice's
 * content (ABNs included) is frozen. On a finalized invoice, the download
 * button, which works like "Download PDF" (`downloadInvoiceEinvoice` hands
 * back a presigned `attachment` URL).
 */
export function InvoiceEinvoice({
  invoice,
}: {
  invoice: InvoiceEinvoice_invoice$key;
}) {
  const data = useFragment(invoiceEinvoiceFragment, invoice);
  const [error, setError] = useState<string | null>(null);
  const [commit, isPending] = useMutation<InvoiceEinvoiceDownloadMutation>(
    graphql`
      mutation InvoiceEinvoiceDownloadMutation($invoiceId: ID!) {
        downloadInvoiceEinvoice(invoiceId: $invoiceId)
      }
    `,
  );
  const problems = data.einvoiceProblems;
  const isDraft = data.status === "DRAFT";

  if (problems.length > 0) {
    return (
      <div
        role="status"
        className="rounded-lg border border-amber-300 bg-amber-50 p-4 text-sm text-amber-900 dark:border-amber-700 dark:bg-amber-950 dark:text-amber-200"
      >
        <p className="font-medium">
          {isDraft
            ? "This invoice can't be issued as an e-invoice yet:"
            : "This invoice can't be issued as an e-invoice:"}
        </p>
        <ul className="mt-1 list-disc pl-5">
          {problems.map((problem) => (
            <li key={problem}>{problem}</li>
          ))}
        </ul>
        <p className="mt-2">
          {isDraft
            ? "Fix these before finalizing — a finalized invoice can't change."
            : "It was finalized without them, and a finalized invoice can't change."}
        </p>
      </div>
    );
  }

  if (isDraft) {
    return (
      <p className="text-sm text-ink-muted">
        Ready to issue as an Australian e-invoice (Peppol PINT A-NZ) once
        finalized.
      </p>
    );
  }

  function download() {
    setError(null);
    commit({
      variables: { invoiceId: data.id },
      onCompleted: (result) => {
        window.location.assign(result.downloadInvoiceEinvoice);
      },
      onError: (err) =>
        setError(
          relayMutationErrorMessage(err, "Failed to prepare the e-invoice."),
        ),
    });
  }

  return (
    <div className="flex flex-col gap-2">
      <Button
        variant="secondary"
        className="self-start"
        disabled={isPending}
        onClick={download}
      >
        {isPending ? "Preparing e-invoice…" : "Download e-invoice (XML)"}
      </Button>
      <p className="text-xs text-ink-muted">
        Peppol PINT A-NZ format: deliver it through your Peppol access point, or
        send it to a client whose accounts system imports e-invoices.
      </p>
      {error && (
        <p role="alert" className="text-sm text-red-600 dark:text-red-400">
          {error}
        </p>
      )}
    </div>
  );
}
