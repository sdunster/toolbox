import { Suspense, useState } from "react";
import { graphql } from "react-relay";
import type { ProjectExpensesQuery } from "./__generated__/ProjectExpensesQuery.graphql";
import { useRetryableLazyLoadQuery } from "../../components/useRetryableLazyLoadQuery";
import RelayErrorBoundary from "../../components/RelayErrorBoundary";
import LoadingIndicator from "../../components/LoadingIndicator";
import { ExpenseList } from "./ExpenseList";
import { AddExpenseForm } from "./AddExpenseForm";

const projectExpensesQuery = graphql`
  query ProjectExpensesQuery($instanceId: ID!, $projectId: ID!)
  @throwOnFieldError {
    ...ExpenseList_query
      @arguments(instanceId: $instanceId, projectId: $projectId)
  }
`;

function Expenses({
  instanceId,
  projectId,
  currency,
  refreshKey,
}: {
  instanceId: string;
  projectId: string;
  currency: string;
  refreshKey: number;
}) {
  const data = useRetryableLazyLoadQuery<ProjectExpensesQuery>(
    projectExpensesQuery,
    { instanceId, projectId },
  );
  return (
    <ExpenseList
      query={data}
      category={null}
      showProject={false}
      currency={currency}
      emptyMessage="No expenses on this project yet."
      refreshKey={refreshKey}
    />
  );
}

/**
 * The project page's "Expenses" section: this project's expenses and,
 * unless the project is archived, an "Add expense" form with the project
 * fixed. `instanceId` is the *project's* instance, same reasoning as
 * `ProjectBillableItems`.
 */
export function ProjectExpenses({
  instanceId,
  projectId,
  currency,
  archived,
}: {
  instanceId: string;
  projectId: string;
  currency: string;
  archived: boolean;
}) {
  const [refreshKey, setRefreshKey] = useState(0);
  return (
    <section
      aria-labelledby="project-expenses-heading"
      className="flex flex-col gap-4"
    >
      <h2
        id="project-expenses-heading"
        className="text-lg font-semibold text-ink-strong"
      >
        Expenses
      </h2>
      <RelayErrorBoundary canRetry>
        <Suspense fallback={<LoadingIndicator />}>
          <Expenses
            instanceId={instanceId}
            projectId={projectId}
            currency={currency}
            refreshKey={refreshKey}
          />
        </Suspense>
      </RelayErrorBoundary>
      {archived ? (
        <p className="rounded-lg border border-dashed border-line p-4 text-sm text-ink-muted">
          This project is archived, so it can&apos;t take new expenses.
        </p>
      ) : (
        <div className="max-w-4xl">
          <AddExpenseForm
            instanceId={instanceId}
            currency={currency}
            projectId={projectId}
            onCreated={() => setRefreshKey((k) => k + 1)}
          />
        </div>
      )}
    </section>
  );
}
