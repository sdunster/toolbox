import { Suspense } from "react";
import { graphql } from "react-relay";
import type { ProjectFinancialsQuery } from "./__generated__/ProjectFinancialsQuery.graphql";
import { useRetryableLazyLoadQuery } from "../../components/useRetryableLazyLoadQuery";
import RelayErrorBoundary from "../../components/RelayErrorBoundary";
import LoadingIndicator from "../../components/LoadingIndicator";
import { formatCents } from "../../lib/money";

const projectFinancialsQuery = graphql`
  query ProjectFinancialsQuery($id: ID!) @throwOnFieldError {
    project(id: $id) {
      financials {
        invoicedCents
        paidCents
        outstandingCents
        expensesCents
        unbilledCents
        profitCents
      }
    }
  }
`;

function Stat({
  label,
  cents,
  currency,
  tone,
}: {
  label: string;
  cents: number;
  currency: string;
  tone?: "good" | "bad";
}) {
  const color =
    tone === "bad"
      ? "text-red-700 dark:text-red-400"
      : tone === "good"
        ? "text-green-700 dark:text-green-400"
        : "text-ink-strong";
  return (
    <div className="rounded-lg border border-line bg-surface p-3">
      <dt className="text-xs font-medium tracking-wide text-ink-muted uppercase">
        {label}
      </dt>
      <dd className={`mt-1 text-lg font-semibold tabular-nums ${color}`}>
        {formatCents(cents)}{" "}
        <span className="text-xs font-normal text-ink-muted">{currency}</span>
      </dd>
    </div>
  );
}

function Content({ id, currency }: { id: string; currency: string }) {
  const data = useRetryableLazyLoadQuery<ProjectFinancialsQuery>(
    projectFinancialsQuery,
    { id },
    { fetchPolicy: "store-and-network" },
  );
  const f = data.project?.financials;
  if (!f) return null;
  return (
    <dl className="grid grid-cols-2 gap-3 sm:grid-cols-3 lg:grid-cols-6">
      <Stat
        label="Invoiced (ex GST)"
        cents={f.invoicedCents}
        currency={currency}
      />
      <Stat label="Paid" cents={f.paidCents} currency={currency} />
      <Stat
        label="Outstanding"
        cents={f.outstandingCents}
        currency={currency}
        tone={f.outstandingCents > 0 ? "bad" : undefined}
      />
      <Stat label="Unbilled" cents={f.unbilledCents} currency={currency} />
      <Stat
        label="Expenses (ex GST)"
        cents={f.expensesCents}
        currency={currency}
      />
      <Stat
        label="Profit"
        cents={f.profitCents}
        currency={currency}
        tone={f.profitCents < 0 ? "bad" : "good"}
      />
    </dl>
  );
}

/**
 * A project's money at a glance: what it has invoiced (net of credit
 * notes), been paid, is still owed, has waiting to be billed, and has
 * cost — from `Project.financials`.
 */
export function ProjectFinancials({
  projectId,
  currency,
}: {
  projectId: string;
  currency: string;
}) {
  return (
    <section aria-label="Project financials">
      <RelayErrorBoundary canRetry>
        <Suspense fallback={<LoadingIndicator />}>
          <Content id={projectId} currency={currency} />
        </Suspense>
      </RelayErrorBoundary>
    </section>
  );
}
