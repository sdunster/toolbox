import { Suspense } from "react";
import { graphql } from "react-relay";
import { Link, useParams } from "react-router";
import type { InvoiceDetailPageQuery } from "./__generated__/InvoiceDetailPageQuery.graphql";
import { useRetryableLazyLoadQuery } from "../../components/useRetryableLazyLoadQuery";
import RelayErrorBoundary from "../../components/RelayErrorBoundary";
import LoadingIndicator from "../../components/LoadingIndicator";
import { ButtonLink } from "../../components/ui/Button";
import { RequireInvoicingInstance } from "./RequireInvoicingInstance";
import { InvoicePreview } from "./InvoicePreview";
import { InvoiceDraftPanel } from "./InvoiceDraftPanel";
import { InvoicePaidControl } from "./InvoicePaidControl";
import { InvoiceSendPanel } from "./InvoiceSendPanel";
import { InvoiceCreditNotes } from "./InvoiceCreditNotes";

const invoiceDetailPageQuery = graphql`
  query InvoiceDetailPageQuery($id: ID!) @throwOnFieldError {
    invoice(id: $id) {
      id
      status
      displayNumber
      project {
        id
        name
        instance {
          id
        }
      }
      ...InvoicePreview_invoice
      ...InvoiceDraftPanel_invoice
      ...InvoicePaidControl_invoice
      ...InvoiceSendPanel_invoice
      ...InvoiceCreditNotes_invoice
    }
  }
`;

function Content({ id, isOwner }: { id: string; isOwner: boolean }) {
  const data = useRetryableLazyLoadQuery<InvoiceDetailPageQuery>(
    invoiceDetailPageQuery,
    { id },
  );

  if (!data.invoice) {
    return (
      <div className="rounded-lg border border-dashed border-line p-10 text-center text-ink-muted">
        <p className="font-medium text-ink">Invoice not found</p>
        <p className="mt-1 text-sm">
          It may not exist, or you may not have access to it.
        </p>
        <ButtonLink to="/app/invoices" variant="secondary" className="mt-4">
          Back to invoices
        </ButtonLink>
      </div>
    );
  }

  const invoice = data.invoice;

  return (
    <div className="flex max-w-4xl flex-col gap-6">
      <ButtonLink
        to="/app/invoices"
        variant="ghost"
        className="self-start px-0"
      >
        ← Back to invoices
      </ButtonLink>

      <div>
        <h1 className="text-xl font-semibold text-ink-strong">
          {invoice.status === "DRAFT"
            ? "Draft invoice"
            : `Invoice ${invoice.displayNumber}`}
        </h1>
        <Link
          to={`/app/projects/${invoice.project.id}`}
          className="mt-1 text-sm text-accent hover:underline"
        >
          {invoice.project.name}
        </Link>
      </div>

      <InvoicePreview invoice={invoice} />

      {invoice.status === "DRAFT" ? (
        <InvoiceDraftPanel
          invoice={invoice}
          instanceId={invoice.project.instance.id}
          isOwner={isOwner}
        />
      ) : (
        <>
          <InvoicePaidControl invoice={invoice} />
          <InvoiceSendPanel invoice={invoice} />
          <InvoiceCreditNotes invoice={invoice} />
        </>
      )}
    </div>
  );
}

/**
 * `/app/invoices/:id` — one invoice's printed preview plus its
 * status-appropriate actions: a draft gets item management, delete and
 * finalize (`InvoiceDraftPanel`); a finalized invoice is read-only, and gets
 * its payments (`InvoicePaidControl`), emailing (`InvoiceSendPanel`) and
 * credit notes (`InvoiceCreditNotes`) instead.
 * `invoice(id)` is `null` for a missing invoice and for one the caller
 * can't reach alike (see its doc comment), so both show the same "not
 * found" panel. `RequireInvoicingInstance` gates on the *switcher's*
 * selection, same as `ProjectDetailPage` — a deep link can arrive with a
 * different invoicing instance selected than the one this invoice belongs
 * to; `isOwner` (used only to decide whether a business-settings error
 * links there) reflects that selection, not necessarily this invoice's own
 * instance, mirroring the same approximation `ProjectBillableItems` makes
 * for `currency`.
 */
export function InvoiceDetailPage() {
  const { id } = useParams<{ id: string }>();
  if (!id) return null;

  return (
    <RequireInvoicingInstance purpose="see its invoices">
      {(instance) => (
        <RelayErrorBoundary canRetry>
          <Suspense fallback={<LoadingIndicator />}>
            <Content id={id} isOwner={instance.role === "OWNER"} />
          </Suspense>
        </RelayErrorBoundary>
      )}
    </RequireInvoicingInstance>
  );
}
