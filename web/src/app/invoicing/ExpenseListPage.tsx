import { Suspense, useMemo, useState } from "react";
import { graphql } from "react-relay";
import { useSearchParams } from "react-router";
import type {
  ExpenseCategoryType,
  ExpenseListPageQuery,
} from "./__generated__/ExpenseListPageQuery.graphql";
import { useRetryableLazyLoadQuery } from "../../components/useRetryableLazyLoadQuery";
import RelayErrorBoundary from "../../components/RelayErrorBoundary";
import LoadingIndicator from "../../components/LoadingIndicator";
import { inputBase } from "../../components/ui/inputStyles";
import { localToday } from "../../lib/dates";
import { EXPENSE_CATEGORIES, financialYearOf } from "../../lib/expenses";
import { RequireInvoicingInstance } from "./RequireInvoicingInstance";
import { ExpenseList } from "./ExpenseList";
import { AddExpenseForm } from "./AddExpenseForm";
import { VehicleKmSummary } from "./VehicleKmSummary";

// `projects { name }` is read by `ExpenseForm`'s project picker, reached
// through props rather than a fragment, so the lint rule can't see it.
/* eslint-disable relay/unused-fields */
const expenseListPageQuery = graphql`
  query ExpenseListPageQuery(
    $instanceId: ID!
    $slug: String!
    $category: ExpenseCategoryType
  ) @throwOnFieldError {
    instance(slug: $slug) {
      id
      invoicingSettings {
        currency
      }
    }
    projects(instanceId: $instanceId, includeArchived: true) {
      id
      name
      archived
    }
    ...ExpenseList_query
      @arguments(instanceId: $instanceId, category: $category)
  }
`;
/* eslint-enable relay/unused-fields */

function categoryFromParam(param: string | null): ExpenseCategoryType | null {
  return (
    EXPENSE_CATEGORIES.find((c) => c.value === param?.toUpperCase())?.value ??
    null
  );
}

function Content({
  instanceId,
  slug,
  category,
}: {
  instanceId: string;
  slug: string;
  category: ExpenseCategoryType | null;
}) {
  // Captured once per mount; later category changes refetch through
  // `ExpenseList`, like `BillableItemListPage`'s filter.
  const [initialCategory] = useState(category);
  const [refreshKey, setRefreshKey] = useState(0);
  const data = useRetryableLazyLoadQuery<ExpenseListPageQuery>(
    expenseListPageQuery,
    { instanceId, slug, category: initialCategory },
  );
  const currency = data.instance?.invoicingSettings?.currency ?? "AUD";
  // The edit form lists every project, archived included, so an expense on
  // a since-archived job keeps its project; a new expense can only pick an
  // active one (the API refuses an archived project for it anyway).
  const allProjects = data.projects;
  const activeProjects = useMemo(
    () => allProjects.filter((p) => !p.archived),
    [allProjects],
  );
  const financialYear = financialYearOf(localToday()) ?? 0;
  const bump = () => setRefreshKey((k) => k + 1);

  return (
    <div className="flex flex-col gap-4">
      <RelayErrorBoundary canRetry>
        <Suspense fallback={<LoadingIndicator />}>
          <VehicleKmSummary
            instanceId={instanceId}
            financialYear={financialYear}
            refreshKey={refreshKey}
          />
        </Suspense>
      </RelayErrorBoundary>
      <div className="max-w-4xl">
        <AddExpenseForm
          instanceId={instanceId}
          currency={currency}
          projects={activeProjects}
          onCreated={bump}
        />
      </div>
      <ExpenseList
        query={data}
        category={category}
        showProject
        currency={currency}
        projects={allProjects}
        emptyMessage={
          category ? "No expenses in this category." : "No expenses yet."
        }
        refreshKey={refreshKey}
        onChanged={bump}
      />
    </div>
  );
}

/**
 * `/app/expenses` — every expense in the selected invoicing instance,
 * newest date first, filterable by category (`?category=vehicle_km`, so a
 * filtered view can be bookmarked), with the caller's vehicle km running
 * total and an "Add expense" form.
 */
export function ExpenseListPage() {
  const [searchParams, setSearchParams] = useSearchParams();
  const category = categoryFromParam(searchParams.get("category"));

  return (
    <RequireInvoicingInstance purpose="see its expenses">
      {(instance) => (
        <div className="flex flex-col gap-4">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <h1 className="text-xl font-semibold text-ink-strong">Expenses</h1>
            <div className="w-64">
              <label htmlFor="expense-category-filter" className="sr-only">
                Filter by category
              </label>
              <select
                id="expense-category-filter"
                className={inputBase}
                value={category ?? ""}
                onChange={(e) =>
                  setSearchParams(
                    e.target.value
                      ? { category: e.target.value.toLowerCase() }
                      : {},
                    { replace: true },
                  )
                }
              >
                <option value="">All categories</option>
                {EXPENSE_CATEGORIES.map((c) => (
                  <option key={c.value} value={c.value}>
                    {c.label}
                  </option>
                ))}
              </select>
            </div>
          </div>
          <RelayErrorBoundary canRetry>
            <Suspense fallback={<LoadingIndicator />}>
              <Content
                key={instance.id}
                instanceId={instance.id}
                slug={instance.slug}
                category={category}
              />
            </Suspense>
          </RelayErrorBoundary>
        </div>
      )}
    </RequireInvoicingInstance>
  );
}
