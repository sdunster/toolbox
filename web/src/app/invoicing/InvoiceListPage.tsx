import { Suspense, useState } from "react";
import { graphql } from "react-relay";
import { useSearchParams } from "react-router";
import type {
  InvoiceFilterType,
  InvoiceListPageQuery,
} from "./__generated__/InvoiceListPageQuery.graphql";
import { useRetryableLazyLoadQuery } from "../../components/useRetryableLazyLoadQuery";
import RelayErrorBoundary from "../../components/RelayErrorBoundary";
import LoadingIndicator from "../../components/LoadingIndicator";
import { RequireInvoicingInstance } from "./RequireInvoicingInstance";
import { InvoiceList } from "./InvoiceList";

const invoiceListPageQuery = graphql`
  query InvoiceListPageQuery($instanceId: ID!, $filter: InvoiceFilterType!)
  @throwOnFieldError {
    ...InvoiceList_query @arguments(instanceId: $instanceId, filter: $filter)
  }
`;

const FILTERS: Array<{
  value: InvoiceFilterType;
  param: string;
  label: string;
  empty: string;
}> = [
  { value: "ALL", param: "all", label: "All", empty: "No invoices yet." },
  {
    value: "DRAFT",
    param: "draft",
    label: "Draft",
    empty: "No drafts — start one from a project's billable items.",
  },
  {
    value: "UNPAID",
    param: "unpaid",
    label: "Unpaid",
    empty: "Nothing unpaid — every finalized invoice is settled.",
  },
  {
    value: "OVERDUE",
    param: "overdue",
    label: "Overdue",
    empty: "Nothing overdue.",
  },
  {
    value: "PAID",
    param: "paid",
    label: "Paid",
    empty: "No invoices marked paid yet.",
  },
];

function filterFromParam(param: string | null) {
  return FILTERS.find((f) => f.param === param) ?? FILTERS[0];
}

/**
 * All / Draft / Unpaid / Overdue / Paid. Buttons with `aria-pressed` rather than a
 * tablist — they filter one list in place, they don't switch panels, same
 * convention as `BillableItemListPage`'s tabs.
 */
function FilterTabs({
  current,
  onChange,
}: {
  current: InvoiceFilterType;
  onChange: (param: string) => void;
}) {
  return (
    <div
      role="group"
      aria-label="Filter invoices"
      className="inline-flex rounded-md border border-line bg-surface p-0.5"
    >
      {FILTERS.map((f) => {
        const active = f.value === current;
        return (
          <button
            key={f.value}
            type="button"
            aria-pressed={active}
            onClick={() => onChange(f.param)}
            className={[
              "cursor-pointer rounded px-3 py-1.5 text-sm font-medium transition-colors",
              active
                ? "bg-accent text-white"
                : "text-ink-muted hover:bg-surface-raised hover:text-ink",
            ].join(" ")}
          >
            {f.label}
          </button>
        );
      })}
    </div>
  );
}

function Content({
  instanceId,
  filter,
}: {
  instanceId: string;
  filter: (typeof FILTERS)[number];
}) {
  // Captured once per mount (per instance, via the `key` below); later
  // filter changes go through `InvoiceList`'s own refetch.
  const [initialFilter] = useState(filter.value);
  // `store-and-network`, same reasoning as `ProjectListPage`: `invoices` is
  // read through a paginated `@connection`, so a draft created (or
  // finalized) elsewhere isn't spliced into a cached copy of it — coming
  // back to this page shows the cached list instantly and then refreshes it.
  const data = useRetryableLazyLoadQuery<InvoiceListPageQuery>(
    invoiceListPageQuery,
    { instanceId, filter: initialFilter },
    { fetchPolicy: "store-and-network" },
  );

  return (
    <InvoiceList
      query={data}
      filter={filter.value}
      showProject
      emptyMessage={filter.empty}
    />
  );
}

/**
 * `/app/invoices` — every invoice in the selected invoicing instance,
 * newest first, filterable by status (`?filter=` `draft`/`unpaid`/`overdue`/`paid`,
 * so a filtered view can be bookmarked). Invoices are created from a
 * project's unbilled items; this list links to each one's detail page.
 */
export function InvoiceListPage() {
  const [searchParams, setSearchParams] = useSearchParams();
  const filter = filterFromParam(searchParams.get("filter"));

  return (
    <RequireInvoicingInstance purpose="see its invoices">
      {(instance) => (
        <div className="flex flex-col gap-4">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <h1 className="text-xl font-semibold text-ink-strong">Invoices</h1>
            <FilterTabs
              current={filter.value}
              onChange={(param) =>
                setSearchParams(param === "all" ? {} : { filter: param }, {
                  replace: true,
                })
              }
            />
          </div>
          <RelayErrorBoundary canRetry>
            <Suspense fallback={<LoadingIndicator />}>
              <Content
                key={instance.id}
                instanceId={instance.id}
                filter={filter}
              />
            </Suspense>
          </RelayErrorBoundary>
        </div>
      )}
    </RequireInvoicingInstance>
  );
}
