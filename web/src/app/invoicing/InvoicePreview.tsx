import { graphql, useFragment } from "react-relay";
import type { InvoicePreview_invoice$key } from "./__generated__/InvoicePreview_invoice.graphql";
import { formatDate } from "../../lib/dates";
import { formatCents } from "../../lib/money";

const invoicePreviewFragment = graphql`
  fragment InvoicePreview_invoice on Invoice {
    title
    displayNumber
    issueDate
    dueDate
    billTo {
      name
      abn
      address
    }
    reference
    seller {
      name
      abn
      address
      phone
      email
    }
    lines {
      description
      quantity
      unitPriceCents
      amountCents
      gstFree
    }
    subtotalCents
    gstCents
    totalCents
    currency
    gstRegistered
    paymentDetails
  }
`;

/** Plain lines, one `<p>` each — for an address or similar free text. */
function TextBlock({ text, className }: { text: string; className?: string }) {
  return (
    <>
      {text.split("\n").map((line, i) => (
        <p key={i} className={className}>
          {line}
        </p>
      ))}
    </>
  );
}

/**
 * A billable item's description, printed the way the build plan specifies:
 * a line starting `* ` or `- ` becomes a bullet, everything else is a plain
 * paragraph.
 */
function DescriptionLines({ text }: { text: string }) {
  return (
    <div className="flex flex-col gap-0.5">
      {text.split("\n").map((line, i) => {
        const bullet = line.startsWith("* ") || line.startsWith("- ");
        return bullet ? (
          <p key={i} className="flex gap-1.5 pl-1 wrap-break-word">
            <span aria-hidden="true">•</span>
            <span>{line.slice(2)}</span>
          </p>
        ) : (
          <p key={i} className="wrap-break-word">
            {line}
          </p>
        );
      })}
    </div>
  );
}

/**
 * The invoice, laid out the way it prints — a "paper" card, always
 * white-on-dark-text regardless of the app's own theme (see this file's
 * `bg-white`/`text-neutral-*` classes, deliberately with no `dark:`
 * counterparts), so it reads as a document rather than another panel of
 * the app. Draft and finalized invoices share this exact component: both
 * come from `build_snapshot` server-side (live for a draft, frozen for a
 * finalized one), so the shape is identical — only `displayNumber`/
 * `issueDate` being `null` (rendered as "Draft") tells them apart here.
 *
 * Every optional field (`billTo.abn`, `reference`, `seller.phone`, …) is
 * simply omitted when absent, rather than shown blank.
 */
export function InvoicePreview({
  invoice,
}: {
  invoice: InvoicePreview_invoice$key;
}) {
  const data = useFragment(invoicePreviewFragment, invoice);
  const hasContact = Boolean(data.seller.phone || data.seller.email);

  return (
    <div className="rounded-lg border border-line bg-white p-6 text-neutral-900 shadow-sm sm:p-10">
      <h2 className="text-2xl font-bold sm:text-3xl">{data.title}</h2>

      <div className="mt-8 grid gap-8 text-sm sm:grid-cols-2">
        <div className="flex flex-col gap-4">
          <div>
            <p>
              Invoice date:{" "}
              {data.issueDate ? formatDate(data.issueDate) : "Draft"}
            </p>
            {data.dueDate && (
              <p className="font-bold">Due date: {formatDate(data.dueDate)}</p>
            )}
            <p>Invoice number: {data.displayNumber ?? "Draft"}</p>
          </div>
          <div>
            <p className="font-bold">Invoice to</p>
            <p>{data.billTo.name}</p>
            {data.billTo.abn && <p>ABN: {data.billTo.abn}</p>}
            {data.billTo.address && <TextBlock text={data.billTo.address} />}
          </div>
          {data.reference && (
            <div>
              <p className="font-bold">Reference</p>
              <p>{data.reference}</p>
            </div>
          )}
        </div>

        <div className="flex flex-col gap-4 sm:items-end sm:text-right">
          {(data.seller.name || data.seller.address) && (
            <div>
              {data.seller.name && <p>{data.seller.name}</p>}
              {data.seller.address && <TextBlock text={data.seller.address} />}
            </div>
          )}
          {hasContact && (
            <div>
              <p className="font-bold">Contact</p>
              {data.seller.phone && <p>{data.seller.phone}</p>}
              {data.seller.email && <p>{data.seller.email}</p>}
            </div>
          )}
          {data.seller.abn && (
            <div>
              <p className="font-bold">ABN</p>
              <p>{data.seller.abn}</p>
            </div>
          )}
        </div>
      </div>

      <div className="mt-8 overflow-x-auto">
        <table className="w-full min-w-lg border-collapse text-sm">
          <thead>
            <tr className="border-b border-neutral-300 text-left text-xs font-semibold tracking-wide text-neutral-600 uppercase">
              <th scope="col" className="py-2 pr-3">
                Description
              </th>
              <th scope="col" className="py-2 pr-3 text-right">
                Quantity
              </th>
              <th scope="col" className="py-2 pr-3 text-right">
                Unit Price
              </th>
              <th scope="col" className="py-2 text-right">
                {data.currency}
              </th>
            </tr>
          </thead>
          <tbody>
            {data.lines.map((line, i) => (
              <tr key={i} className="border-b border-neutral-200 align-top">
                <td className="py-2 pr-3">
                  <DescriptionLines text={line.description} />
                  {data.gstRegistered && line.gstFree && (
                    <p className="text-xs text-neutral-600">(GST-free)</p>
                  )}
                </td>
                <td className="py-2 pr-3 text-right tabular-nums">
                  {line.quantity}
                </td>
                <td className="py-2 pr-3 text-right tabular-nums">
                  {formatCents(line.unitPriceCents)}
                </td>
                <td className="py-2 text-right tabular-nums">
                  {formatCents(line.amountCents)}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      <div className="mt-4 flex flex-col items-end gap-1 text-sm">
        {data.gstRegistered ? (
          <>
            <p>
              Subtotal {data.currency}:{" "}
              <span className="tabular-nums">
                {formatCents(data.subtotalCents)}
              </span>
            </p>
            <p>
              GST (10%):{" "}
              <span className="tabular-nums">{formatCents(data.gstCents)}</span>
            </p>
            <p className="font-semibold">
              Total {data.currency}:{" "}
              <span className="tabular-nums">
                {formatCents(data.totalCents)}
              </span>
            </p>
          </>
        ) : (
          <p className="font-semibold">
            Total {data.currency}:{" "}
            <span className="tabular-nums">{formatCents(data.totalCents)}</span>
          </p>
        )}
      </div>

      {!data.gstRegistered && (
        <p className="mt-4 text-sm text-neutral-600">
          No GST has been charged.
        </p>
      )}

      {data.paymentDetails && (
        <div className="mt-8 border-t border-neutral-200 pt-4 text-sm whitespace-pre-line text-neutral-700">
          {data.paymentDetails}
        </div>
      )}
    </div>
  );
}
