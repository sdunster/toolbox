import { useEffect, useRef, useTransition } from "react";
import { graphql, usePaginationFragment } from "react-relay";
import type { ExpenseList_query$key } from "./__generated__/ExpenseList_query.graphql";
import type {
  ExpenseCategoryType,
  ExpenseListPaginationQuery,
} from "./__generated__/ExpenseListPaginationQuery.graphql";
import { Button } from "../../components/ui/Button";
import { ExpenseHeader, ExpenseRow } from "./ExpenseRow";
import type { ExpenseProjectOption } from "./ExpenseForm";
import type { BrandOption } from "../../lib/brands";

export const EXPENSE_PAGE_SIZE = 25;

const expenseListFragment = graphql`
  fragment ExpenseList_query on QueryRoot
  @refetchable(queryName: "ExpenseListPaginationQuery")
  @argumentDefinitions(
    instanceId: { type: "ID!" }
    projectId: { type: "ID" }
    category: { type: "ExpenseCategoryType" }
    brandId: { type: "ID" }
    count: { type: "Int", defaultValue: 25 }
    cursor: { type: "String" }
  ) {
    expenses(
      instanceId: $instanceId
      projectId: $projectId
      category: $category
      brandId: $brandId
      first: $count
      after: $cursor
    ) @connection(key: "ExpenseList_expenses") {
      __id
      edges {
        node {
          id
          ...ExpenseRow_expense
        }
      }
    }
  }
`;

/**
 * A paginated list of expenses, newest date first, with "Load more". Owns
 * its refetching like `BillableItemList`: a new `category` or `brandId`, or
 * a bumped `refreshKey` after the parent adds one, refetches in a transition
 * back to the first page (where a new or re-dated expense lands).
 */
export function ExpenseList({
  query,
  category,
  brandId = null,
  showProject,
  currency,
  projects,
  brands,
  emptyMessage,
  refreshKey = 0,
  onChanged,
}: {
  query: ExpenseList_query$key;
  category: ExpenseCategoryType | null;
  brandId?: string | null;
  showProject: boolean;
  currency: string;
  projects?: ReadonlyArray<ExpenseProjectOption>;
  brands?: ReadonlyArray<BrandOption>;
  emptyMessage: string;
  refreshKey?: number;
  /** After an edit — e.g. so the page's km total can refresh too. */
  onChanged?: () => void;
}) {
  const { data, loadNext, hasNext, isLoadingNext, refetch } =
    usePaginationFragment<ExpenseListPaginationQuery, ExpenseList_query$key>(
      expenseListFragment,
      query,
    );
  const [isRefetching, startTransition] = useTransition();

  const mounted = useRef({ category, brandId, refreshKey });
  useEffect(() => {
    if (
      mounted.current.category === category &&
      mounted.current.brandId === brandId &&
      mounted.current.refreshKey === refreshKey
    ) {
      return;
    }
    mounted.current = { category, brandId, refreshKey };
    startTransition(() => {
      refetch({ category, brandId }, { fetchPolicy: "network-only" });
    });
  }, [category, brandId, refreshKey, refetch]);

  const connection = data.expenses;
  const edges = connection.edges;

  if (edges.length === 0) {
    return (
      <div
        className={`rounded-lg border border-dashed border-line p-10 text-center text-ink-muted ${isRefetching ? "opacity-60" : ""}`}
      >
        {emptyMessage}
      </div>
    );
  }

  return (
    <div
      className={`flex flex-col gap-4 ${isRefetching ? "opacity-60" : ""}`}
      aria-busy={isRefetching}
    >
      <div className="rounded-lg border border-line">
        <ExpenseHeader showProject={showProject} currency={currency} />
        <ul className="flex flex-col divide-y divide-line-faint">
          {edges.map((edge) => (
            <li key={edge.node.id}>
              <ExpenseRow
                expense={edge.node}
                showProject={showProject}
                currency={currency}
                projects={projects}
                brands={brands}
                connectionId={connection.__id}
                onChanged={() => {
                  startTransition(() => {
                    refetch(
                      { category, brandId },
                      { fetchPolicy: "network-only" },
                    );
                  });
                  onChanged?.();
                }}
              />
            </li>
          ))}
        </ul>
      </div>

      {hasNext && (
        <Button
          variant="secondary"
          className="self-center"
          disabled={isLoadingNext}
          onClick={() => loadNext(EXPENSE_PAGE_SIZE)}
        >
          {isLoadingNext ? "Loading…" : "Load more"}
        </Button>
      )}
    </div>
  );
}
