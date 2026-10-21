import { graphql, useFragment } from "react-relay";
import { Link } from "react-router";
import type { InvoiceListRow_invoice$key } from "./__generated__/InvoiceListRow_invoice.graphql";
import { formatDate } from "../../lib/dates";
import { formatCents } from "../../lib/money";
import {
  invoiceGridCols,
  invoiceStatusBadgeBase,
  invoiceStatusBadgeClass,
  invoiceStatusLabel,
} from "./invoiceStyles";

const invoiceListRowFragment = graphql`
  fragment InvoiceListRow_invoice on Invoice {
    id
    status
    displayNumber
    issueDate
    paidDate
    overdue
    totalCents
    currency
    project {
      id
      name
    }
  }
`;

/**
 * Column headings for a list of {@link InvoiceListRow}s — lg+ only; below
 * that each row is a self-describing card, same convention as
 * `BillableItemHeader`.
 */
export function InvoiceListHeader({ showProject }: { showProject: boolean }) {
  const cols = showProject
    ? invoiceGridCols.withProject
    : invoiceGridCols.withoutProject;
  return (
    <div
      className={`hidden gap-x-4 border-b border-line px-3 py-2 text-xs font-medium tracking-wide text-ink-muted uppercase lg:grid ${cols}`}
      aria-hidden="true"
    >
      <span>Number</span>
      <span>Issue date</span>
      {showProject && <span>Project</span>}
      <span className="text-right">Total</span>
      <span>Status</span>
    </div>
  );
}

/**
 * One invoice — the instance-wide list (with a project column) or a
 * project's own "Invoices" section (without). The whole row links to
 * `/app/invoices/:id`. `displayNumber` is `null` for a draft, shown here as
 * plain text "Draft" (the status badge separately says the same, more
 * prominently, alongside a paid date once finalized).
 */
export function InvoiceListRow({
  invoice,
  showProject,
}: {
  invoice: InvoiceListRow_invoice$key;
  showProject: boolean;
}) {
  const data = useFragment(invoiceListRowFragment, invoice);
  const cols = showProject
    ? invoiceGridCols.withProject
    : invoiceGridCols.withoutProject;

  return (
    <Link
      to={`/app/invoices/${data.id}`}
      className={`flex flex-col gap-1.5 p-3 no-underline transition-colors hover:bg-surface-raised lg:grid lg:items-center lg:gap-x-4 ${cols}`}
    >
      <div className="flex items-baseline justify-between gap-3 lg:contents">
        <span className="font-medium text-ink-strong tabular-nums lg:order-1">
          {data.displayNumber ?? "Draft"}
        </span>
        <span
          className={`${invoiceStatusBadgeBase} ${invoiceStatusBadgeClass(data.status, data.paidDate, data.overdue)} lg:order-5 lg:justify-self-start`}
        >
          {invoiceStatusLabel(data.status, data.paidDate, data.overdue)}
        </span>
      </div>
      <span className="text-sm text-ink-muted tabular-nums lg:order-2">
        {data.issueDate ? formatDate(data.issueDate) : "—"}
      </span>
      {showProject && (
        <span
          className="min-w-0 truncate text-sm text-ink lg:order-3"
          title={data.project.name}
        >
          {data.project.name}
        </span>
      )}
      <span className="text-sm font-medium text-ink-strong tabular-nums lg:order-4 lg:text-right">
        {formatCents(data.totalCents)}
        <span className="text-ink-muted"> {data.currency}</span>
      </span>
    </Link>
  );
}
