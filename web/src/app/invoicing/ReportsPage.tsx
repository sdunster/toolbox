import { Suspense, useState } from "react";
import { fetchQuery, graphql, useRelayEnvironment } from "react-relay";
import { Link } from "react-router";
import type {
  ReportBasisType,
  ReportsPageGstQuery,
} from "./__generated__/ReportsPageGstQuery.graphql";
import type { ReportsPageReceivablesQuery } from "./__generated__/ReportsPageReceivablesQuery.graphql";
import type {
  CsvExportType,
  ReportsPageExportQuery,
} from "./__generated__/ReportsPageExportQuery.graphql";
import { useRetryableLazyLoadQuery } from "../../components/useRetryableLazyLoadQuery";
import RelayErrorBoundary from "../../components/RelayErrorBoundary";
import LoadingIndicator from "../../components/LoadingIndicator";
import { Card } from "../../components/ui/Card";
import { Button } from "../../components/ui/Button";
import { FormField } from "../../components/ui/FormField";
import TextInput from "../../components/ui/TextInput";
import { formatDate, localToday } from "../../lib/dates";
import { formatCents } from "../../lib/money";
import { presetPeriods } from "../../lib/periods";
import { relayMutationErrorMessage } from "../../lib/relayMutationError";
import { RequireInvoicingInstance } from "./RequireInvoicingInstance";

const gstQuery = graphql`
  query ReportsPageGstQuery(
    $instanceId: ID!
    $from: String!
    $to: String!
    $basis: ReportBasisType!
  ) @throwOnFieldError {
    gstReport(instanceId: $instanceId, from: $from, to: $to, basis: $basis) {
      gstRegistered
      currency
      salesCents
      gstOnSalesCents
      purchasesCents
      gstOnPurchasesCents
      netGstCents
      invoiceCount
      creditNoteCount
      paymentCount
      expenseCount
    }
  }
`;

const receivablesQuery = graphql`
  query ReportsPageReceivablesQuery($instanceId: ID!) @throwOnFieldError {
    receivables(instanceId: $instanceId) {
      asOf
      currency
      totalCents
      currentCents
      days1To30Cents
      days31To60Cents
      days61To90Cents
      daysOver90Cents
      invoices {
        id
        displayNumber
        dueDate
        daysOverdue
        balanceCents
        project {
          name
          clientName
        }
      }
    }
  }
`;

const exportQuery = graphql`
  query ReportsPageExportQuery(
    $instanceId: ID!
    $kind: CsvExportType!
    $from: String!
    $to: String!
  ) {
    invoicingExport(instanceId: $instanceId, kind: $kind, from: $from, to: $to)
  }
`;

function Row({
  label,
  hint,
  cents,
  strong,
}: {
  label: string;
  hint?: string;
  cents: number;
  strong?: boolean;
}) {
  return (
    <div
      className={`flex items-baseline justify-between gap-4 py-2 ${strong ? "font-semibold text-ink-strong" : "text-ink"}`}
    >
      <dt>
        {label}
        {hint && <span className="ml-2 text-xs text-ink-muted">{hint}</span>}
      </dt>
      <dd className="tabular-nums">{formatCents(cents)}</dd>
    </div>
  );
}

function GstReport({
  instanceId,
  from,
  to,
  basis,
}: {
  instanceId: string;
  from: string;
  to: string;
  basis: ReportBasisType;
}) {
  const data = useRetryableLazyLoadQuery<ReportsPageGstQuery>(
    gstQuery,
    { instanceId, from, to, basis },
    { fetchPolicy: "network-only" },
  );
  const r = data.gstReport;
  return (
    <div className="flex flex-col gap-2">
      {!r.gstRegistered && (
        <p className="text-sm text-amber-700 dark:text-amber-400">
          This business isn&apos;t set as registered for GST, so there&apos;s no
          GST to report — the sales figure still stands.
        </p>
      )}
      <dl className="divide-y divide-line text-sm">
        <Row label="Total sales" hint="G1, incl. GST" cents={r.salesCents} />
        <Row label="GST on sales" hint="1A" cents={r.gstOnSalesCents} />
        <Row label="Purchases" hint="incl. GST" cents={r.purchasesCents} />
        <Row label="GST on purchases" hint="1B" cents={r.gstOnPurchasesCents} />
        <Row
          label={r.netGstCents >= 0 ? "GST payable" : "GST refund"}
          hint={r.currency}
          cents={Math.abs(r.netGstCents)}
          strong
        />
      </dl>
      <p className="text-xs text-ink-muted">
        From {r.invoiceCount} invoice{r.invoiceCount === 1 ? "" : "s"}
        {basis === "CASH"
          ? `, ${r.paymentCount} payment${r.paymentCount === 1 ? "" : "s"}`
          : `, ${r.creditNoteCount} credit note${r.creditNoteCount === 1 ? "" : "s"}`}{" "}
        and {r.expenseCount} expense{r.expenseCount === 1 ? "" : "s"}. A guide,
        not tax advice — check with your accountant before lodging.
      </p>
    </div>
  );
}

function Receivables({ instanceId }: { instanceId: string }) {
  const data = useRetryableLazyLoadQuery<ReportsPageReceivablesQuery>(
    receivablesQuery,
    { instanceId },
    { fetchPolicy: "store-and-network" },
  );
  const r = data.receivables;
  const buckets = [
    { label: "Not yet due", cents: r.currentCents },
    { label: "1–30 days", cents: r.days1To30Cents },
    { label: "31–60 days", cents: r.days31To60Cents },
    { label: "61–90 days", cents: r.days61To90Cents },
    { label: "Over 90 days", cents: r.daysOver90Cents },
  ];
  return (
    <div className="flex flex-col gap-4">
      <dl className="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-6">
        <div className="rounded-lg border border-line p-3">
          <dt className="text-xs font-medium text-ink-muted uppercase">
            Total owed
          </dt>
          <dd className="mt-1 text-lg font-semibold tabular-nums">
            {formatCents(r.totalCents)}
          </dd>
        </div>
        {buckets.map((b) => (
          <div key={b.label} className="rounded-lg border border-line p-3">
            <dt className="text-xs font-medium text-ink-muted uppercase">
              {b.label}
            </dt>
            <dd className="mt-1 text-lg font-semibold tabular-nums">
              {formatCents(b.cents)}
            </dd>
          </div>
        ))}
      </dl>
      <p className="text-xs text-ink-muted">As of {formatDate(r.asOf)}.</p>
      {r.invoices.length === 0 ? (
        <p className="text-sm text-ink-muted">Nothing owed. 🎉</p>
      ) : (
        <ul className="divide-y divide-line rounded-md border border-line">
          {r.invoices.map((inv) => (
            <li key={inv.id}>
              <Link
                to={`/app/invoices/${inv.id}`}
                className="flex flex-wrap items-baseline justify-between gap-2 px-3 py-2 text-sm no-underline hover:bg-surface-raised"
              >
                <span className="font-medium text-ink-strong">
                  {inv.displayNumber}{" "}
                  <span className="font-normal text-ink-muted">
                    {inv.project.clientName} · {inv.project.name}
                  </span>
                </span>
                <span className="flex gap-4">
                  <span
                    className={
                      inv.daysOverdue
                        ? "text-red-700 dark:text-red-400"
                        : "text-ink-muted"
                    }
                  >
                    {inv.daysOverdue
                      ? `${inv.daysOverdue} days overdue`
                      : inv.dueDate
                        ? `Due ${formatDate(inv.dueDate)}`
                        : ""}
                  </span>
                  <span className="font-medium tabular-nums">
                    {formatCents(inv.balanceCents)} {r.currency}
                  </span>
                </span>
              </Link>
            </li>
          ))}
        </ul>
      )}
    </div>
  );
}

const EXPORTS: Array<{ kind: CsvExportType; label: string; file: string }> = [
  { kind: "INVOICES", label: "Invoices", file: "invoices" },
  { kind: "PAYMENTS", label: "Payments", file: "payments" },
  { kind: "CREDIT_NOTES", label: "Credit notes", file: "credit-notes" },
  { kind: "EXPENSES", label: "Expenses", file: "expenses" },
];

function Exports({
  instanceId,
  from,
  to,
}: {
  instanceId: string;
  from: string;
  to: string;
}) {
  const environment = useRelayEnvironment();
  const [busy, setBusy] = useState<CsvExportType | null>(null);
  const [error, setError] = useState<string | null>(null);

  async function download(kind: CsvExportType, file: string) {
    setBusy(kind);
    setError(null);
    try {
      const data = await fetchQuery<ReportsPageExportQuery>(
        environment,
        exportQuery,
        { instanceId, kind, from, to },
        { fetchPolicy: "network-only" },
      ).toPromise();
      const blob = new Blob([data?.invoicingExport ?? ""], {
        type: "text/csv;charset=utf-8",
      });
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = `${file}-${from}-to-${to}.csv`;
      a.click();
      URL.revokeObjectURL(url);
    } catch (err) {
      setError(
        err instanceof Error
          ? relayMutationErrorMessage(err, "Failed to export.")
          : "Failed to export.",
      );
    } finally {
      setBusy(null);
    }
  }

  return (
    <div className="flex flex-col gap-2">
      <div className="flex flex-wrap gap-2">
        {EXPORTS.map((e) => (
          <Button
            key={e.kind}
            variant="secondary"
            disabled={busy !== null}
            onClick={() => void download(e.kind, e.file)}
          >
            {busy === e.kind ? "Exporting…" : `${e.label} CSV`}
          </Button>
        ))}
      </div>
      {error && (
        <p role="alert" className="text-sm text-red-600 dark:text-red-400">
          {error}
        </p>
      )}
    </div>
  );
}

function Content({ instanceId }: { instanceId: string }) {
  const presets = presetPeriods(localToday());
  const [from, setFrom] = useState(presets[1].from);
  const [to, setTo] = useState(presets[1].to);
  const [basis, setBasis] = useState<ReportBasisType>("CASH");

  return (
    <div className="flex max-w-5xl flex-col gap-6">
      <Card>
        <h2 className="mb-4 text-sm font-semibold tracking-wide text-ink-muted uppercase">
          Period
        </h2>
        <div className="flex flex-wrap gap-2">
          {presets.map((p) => (
            <Button
              key={p.label}
              variant={p.from === from && p.to === to ? "primary" : "secondary"}
              onClick={() => {
                setFrom(p.from);
                setTo(p.to);
              }}
            >
              {p.label}
            </Button>
          ))}
        </div>
        <div className="mt-4 flex flex-wrap items-end gap-4">
          <div className="w-44">
            <FormField label="From" htmlFor="report-from">
              <TextInput
                id="report-from"
                type="date"
                value={from}
                onChange={(e) => setFrom(e.target.value)}
              />
            </FormField>
          </div>
          <div className="w-44">
            <FormField label="To" htmlFor="report-to">
              <TextInput
                id="report-to"
                type="date"
                value={to}
                onChange={(e) => setTo(e.target.value)}
              />
            </FormField>
          </div>
        </div>
      </Card>

      <Card>
        <div className="mb-4 flex flex-wrap items-center justify-between gap-3">
          <h2 className="text-sm font-semibold tracking-wide text-ink-muted uppercase">
            GST (BAS)
          </h2>
          <div
            role="group"
            aria-label="Accounting basis"
            className="inline-flex rounded-md border border-line bg-surface p-0.5"
          >
            {(["CASH", "ACCRUAL"] as const).map((b) => (
              <button
                key={b}
                type="button"
                aria-pressed={basis === b}
                onClick={() => setBasis(b)}
                className={[
                  "cursor-pointer rounded px-3 py-1.5 text-sm font-medium",
                  basis === b
                    ? "bg-accent text-white"
                    : "text-ink-muted hover:bg-surface-raised hover:text-ink",
                ].join(" ")}
              >
                {b === "CASH" ? "Cash" : "Accrual"}
              </button>
            ))}
          </div>
        </div>
        {from && to ? (
          <RelayErrorBoundary canRetry>
            <Suspense fallback={<LoadingIndicator />}>
              <GstReport
                instanceId={instanceId}
                from={from}
                to={to}
                basis={basis}
              />
            </Suspense>
          </RelayErrorBoundary>
        ) : (
          <p className="text-sm text-ink-muted">Pick a period.</p>
        )}
      </Card>

      <Card>
        <h2 className="mb-4 text-sm font-semibold tracking-wide text-ink-muted uppercase">
          Export for your accountant
        </h2>
        <p className="mb-3 text-sm text-ink-muted">
          CSV files for the period above.
        </p>
        <Exports instanceId={instanceId} from={from} to={to} />
      </Card>

      <Card>
        <h2 className="mb-4 text-sm font-semibold tracking-wide text-ink-muted uppercase">
          Owed to you
        </h2>
        <RelayErrorBoundary canRetry>
          <Suspense fallback={<LoadingIndicator />}>
            <Receivables instanceId={instanceId} />
          </Suspense>
        </RelayErrorBoundary>
      </Card>
    </div>
  );
}

/**
 * `/app/reports` — the money side of an invoicing instance: the GST/BAS
 * figures for a period (cash or accrual), CSV exports for the same period,
 * and what clients owe now, aged by days past due.
 */
export function ReportsPage() {
  return (
    <RequireInvoicingInstance purpose="see its reports">
      {(instance) => (
        <div className="flex flex-col gap-4">
          <h1 className="text-xl font-semibold text-ink-strong">Reports</h1>
          <Content key={instance.id} instanceId={instance.id} />
        </div>
      )}
    </RequireInvoicingInstance>
  );
}
