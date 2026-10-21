import { useState } from "react";
import { graphql, useMutation } from "react-relay";
import type { InvoiceDownloadButtonMutation } from "./__generated__/InvoiceDownloadButtonMutation.graphql";
import { Button } from "../../components/ui/Button";
import { relayMutationErrorMessage } from "../../lib/relayMutationError";

/**
 * A finalized invoice's "Download PDF" button. `downloadInvoicePdf` hands
 * back a presigned, `Content-Disposition: attachment` S3 URL — landing on it
 * in the same tab (rather than `window.open`) downloads the file without
 * leaving the page and without tripping a popup blocker.
 */
export function InvoiceDownloadButton({ invoiceId }: { invoiceId: string }) {
  const [error, setError] = useState<string | null>(null);
  const [commit, isPending] = useMutation<InvoiceDownloadButtonMutation>(
    graphql`
      mutation InvoiceDownloadButtonMutation($invoiceId: ID!) {
        downloadInvoicePdf(invoiceId: $invoiceId)
      }
    `,
  );

  function download() {
    setError(null);
    commit({
      variables: { invoiceId },
      onCompleted: (data) => {
        window.location.assign(data.downloadInvoicePdf);
      },
      onError: (err) =>
        setError(relayMutationErrorMessage(err, "Failed to prepare the PDF.")),
    });
  }

  return (
    <div className="flex flex-col gap-2">
      <Button variant="secondary" disabled={isPending} onClick={download}>
        {isPending ? "Preparing PDF…" : "Download PDF"}
      </Button>
      {error && (
        <p role="alert" className="text-sm text-red-600 dark:text-red-400">
          {error}
        </p>
      )}
    </div>
  );
}
