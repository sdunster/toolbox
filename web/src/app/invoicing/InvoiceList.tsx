import { useEffect, useRef, useTransition } from "react";
import { graphql, usePaginationFragment } from "react-relay";
import type { InvoiceList_query$key } from "./__generated__/InvoiceList_query.graphql";
import type {
  InvoiceFilterType,
  InvoiceListPaginationQuery,
} from "./__generated__/InvoiceListPaginationQuery.graphql";
import { Button } from "../../components/ui/Button";
import { InvoiceListHeader, InvoiceListRow } from "./InvoiceListRow";

export const INVOICE_PAGE_SIZE = 25;

const invoiceListFragment = graphql`
  fragment InvoiceList_query on QueryRoot
  @refetchable(queryName: "InvoiceListPaginationQuery")
  @argumentDefinitions(
    instanceId: { type: "ID!" }
    projectId: { type: "ID" }
    filter: { type: "InvoiceFilterType!" }
    brandId: { type: "ID" }
    count: { type: "Int", defaultValue: 25 }
    cursor: { type: "String" }
  ) {
    invoices(
      instanceId: $instanceId
      projectId: $projectId
      filter: $filter
      brandId: $brandId
      first: $count
      after: $cursor
    ) @connection(key: "InvoiceList_invoices") {
      edges {
        node {
          id
          ...InvoiceListRow_invoice
        }
      }
    }
  }
`;

/**
 * A paginated list of invoices: the instance-wide page (with a project
 * column) or one project's "Invoices" section (without). Newest
 * `createdAt` first, "Load more" at the bottom — the same refetch-owning
 * pattern as `BillableItemList`: the parent's query runs once per mount,
 * and a new `filter` or `brandId` (or a bumped `refreshKey`) refetches through this
 * fragment's own `refetch` inside a transition, keeping the current rows
 * (dimmed) instead of flashing a Suspense fallback, and going back to the
 * first page.
 */
export function InvoiceList({
  query,
  filter,
  brandId = null,
  showProject,
  emptyMessage,
  refreshKey = 0,
}: {
  query: InvoiceList_query$key;
  filter: InvoiceFilterType;
  brandId?: string | null;
  showProject: boolean;
  emptyMessage: string;
  refreshKey?: number;
}) {
  const { data, loadNext, hasNext, isLoadingNext, refetch } =
    usePaginationFragment<InvoiceListPaginationQuery, InvoiceList_query$key>(
      invoiceListFragment,
      query,
    );
  const [isRefetching, startTransition] = useTransition();

  const mounted = useRef({ filter, brandId, refreshKey });
  useEffect(() => {
    if (
      mounted.current.filter === filter &&
      mounted.current.brandId === brandId &&
      mounted.current.refreshKey === refreshKey
    ) {
      return;
    }
    mounted.current = { filter, brandId, refreshKey };
    startTransition(() => {
      refetch({ filter, brandId }, { fetchPolicy: "network-only" });
    });
  }, [filter, brandId, refreshKey, refetch]);

  const edges = data.invoices.edges;

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
        <InvoiceListHeader showProject={showProject} />
        <ul className="flex flex-col divide-y divide-line-faint">
          {edges.map((edge) => (
            <li key={edge.node.id}>
              <InvoiceListRow invoice={edge.node} showProject={showProject} />
            </li>
          ))}
        </ul>
      </div>

      {hasNext && (
        <Button
          variant="secondary"
          className="self-center"
          disabled={isLoadingNext}
          onClick={() => loadNext(INVOICE_PAGE_SIZE)}
        >
          {isLoadingNext ? "Loading…" : "Load more"}
        </Button>
      )}
    </div>
  );
}
