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
import { InstanceBrandFilter } from "./BrandSelect";
import { ExpenseList } from "./ExpenseList";
import { AddExpenseForm } from "./AddExpenseForm";
import { VehicleKmSummary } from "./VehicleKmSummary";

// `projects { name brand }` and `brands` are read by `ExpenseForm`'s
// pickers, reached through props rather than a fragment, so the lint rule
// can't see them.
/* eslint-disable relay/unused-fields */
const expenseListPageQuery = graphql`
  query ExpenseListPageQuery(
    $instanceId: ID!
    $slug: String!
    $category: ExpenseCategoryType
    $brandId: ID
  ) @throwOnFieldError {
    instance(slug: $slug) {
      id
      invoicingSettings {
        currency
      }
      brands {
        brandId
        name
        archived
        isDefault
      }
    }
    projects(instanceId: $instanceId, includeArchived: true) {
      id
      name
      archived
      brand {
        brandId
      }
    }
    ...ExpenseList_query
      @arguments(
        instanceId: $instanceId
        category: $category
        brandId: $brandId
      )
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
  brandId,
}: {
  instanceId: string;
  slug: string;
  category: ExpenseCategoryType | null;
  brandId: string | null;
}) {
  // Captured once per mount; later filter changes refetch through
  // `ExpenseList`, like `BillableItemListPage`'s filter.
  const [initialCategory] = useState(category);
  const [initialBrandId] = useState(brandId);
  const [refreshKey, setRefreshKey] = useState(0);
  const data = useRetryableLazyLoadQuery<ExpenseListPageQuery>(
    expenseListPageQuery,
    { instanceId, slug, category: initialCategory, brandId: initialBrandId },
  );
  const currency = data.instance?.invoicingSettings?.currency ?? "AUD";
  const brands = data.instance?.brands;
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
          brands={brands}
          onCreated={bump}
        />
      </div>
      <ExpenseList
        query={data}
        category={category}
        brandId={brandId}
        showProject
        currency={currency}
        projects={allProjects}
        brands={brands}
        emptyMessage={
          category || brandId ? "No matching expenses." : "No expenses yet."
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
 * filtered view can be bookmarked) and, once the instance has more than one
 * brand, by brand (`?brand=…`), with the caller's vehicle km running total
 * and an "Add expense" form.
 */
export function ExpenseListPage() {
  const [searchParams, setSearchParams] = useSearchParams();
  const category = categoryFromParam(searchParams.get("category"));
  const brandId = searchParams.get("brand");
  const setParam = (key: string, value: string | null) =>
    setSearchParams(
      (prev) => {
        const next = new URLSearchParams(prev);
        if (value) next.set(key, value);
        else next.delete(key);
        return next;
      },
      { replace: true },
    );

  return (
    <RequireInvoicingInstance purpose="see its expenses">
      {(instance) => (
        <div className="flex flex-col gap-4">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <h1 className="text-xl font-semibold text-ink-strong">Expenses</h1>
            <div className="flex flex-wrap gap-3">
              <InstanceBrandFilter
                slug={instance.slug}
                id="expense-brand-filter"
                value={brandId}
                onChange={(b) => setParam("brand", b)}
                className="w-56"
              />
              <div className="w-64">
                <label htmlFor="expense-category-filter" className="sr-only">
                  Filter by category
                </label>
                <select
                  id="expense-category-filter"
                  className={inputBase}
                  value={category ?? ""}
                  onChange={(e) =>
                    setParam(
                      "category",
                      e.target.value ? e.target.value.toLowerCase() : null,
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
          </div>
          <RelayErrorBoundary canRetry>
            <Suspense fallback={<LoadingIndicator />}>
              <Content
                key={instance.id}
                instanceId={instance.id}
                slug={instance.slug}
                category={category}
                brandId={brandId}
              />
            </Suspense>
          </RelayErrorBoundary>
        </div>
      )}
    </RequireInvoicingInstance>
  );
}
