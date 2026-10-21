import { Suspense, useState } from "react";
import { graphql } from "react-relay";
import { useSearchParams } from "react-router";
import type {
  BillableItemFilterType,
  BillableItemListPageQuery,
} from "./__generated__/BillableItemListPageQuery.graphql";
import { useRetryableLazyLoadQuery } from "../../components/useRetryableLazyLoadQuery";
import RelayErrorBoundary from "../../components/RelayErrorBoundary";
import LoadingIndicator from "../../components/LoadingIndicator";
import { RequireFinanceInstance } from "./RequireFinanceInstance";
import { BillableItemList } from "./BillableItemList";

const billableItemListPageQuery = graphql`
  query BillableItemListPageQuery(
    $instanceId: ID!
    $slug: String!
    $filter: BillableItemFilterType!
  ) @throwOnFieldError {
    instance(slug: $slug) {
      id
      financeSettings {
        currency
      }
    }
    ...BillableItemList_query
      @arguments(instanceId: $instanceId, filter: $filter)
  }
`;

const FILTERS: Array<{
  value: BillableItemFilterType;
  param: string;
  label: string;
  empty: string;
}> = [
  { value: "ALL", param: "all", label: "All", empty: "No billable items yet." },
  {
    value: "UNBILLED",
    param: "unbilled",
    label: "Unbilled",
    empty: "Nothing unbilled — every item is on an invoice.",
  },
  {
    value: "BILLED",
    param: "billed",
    label: "Billed",
    empty: "No items are on an invoice yet.",
  },
];

function filterFromParam(param: string | null) {
  return FILTERS.find((f) => f.param === param) ?? FILTERS[0];
}

/**
 * All / Unbilled / Billed. Buttons with `aria-pressed` rather than a
 * tablist — they filter one list in place, they don't switch panels.
 */
function FilterTabs({
  current,
  onChange,
}: {
  current: BillableItemFilterType;
  onChange: (param: string) => void;
}) {
  return (
    <div
      role="group"
      aria-label="Filter billable items"
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
  slug,
  filter,
}: {
  instanceId: string;
  slug: string;
  filter: (typeof FILTERS)[number];
}) {
  // Captured once per mount (per instance, via the `key` below); later
  // filter changes go through `BillableItemList`'s own refetch, like the
  // ticket queues.
  const [initialFilter] = useState(filter.value);
  const data = useRetryableLazyLoadQuery<BillableItemListPageQuery>(
    billableItemListPageQuery,
    { instanceId, slug, filter: initialFilter },
  );
  const currency = data.instance?.financeSettings?.currency ?? "AUD";

  return (
    <BillableItemList
      query={data}
      filter={filter.value}
      showProject
      editable={false}
      currency={currency}
      emptyMessage={filter.empty}
    />
  );
}

/**
 * `/app/billable-items` — every billable item in the selected finance
 * instance, newest date first, filterable by billed state (`?filter=`
 * `unbilled`/`billed`, so a filtered view can be bookmarked). Items are
 * added and edited on their project's page; this list links there.
 */
export function BillableItemListPage() {
  const [searchParams, setSearchParams] = useSearchParams();
  const filter = filterFromParam(searchParams.get("filter"));

  return (
    <RequireFinanceInstance purpose="see its billable items">
      {(instance) => (
        <div className="flex flex-col gap-4">
          <div className="flex flex-wrap items-center justify-between gap-3">
            <h1 className="text-xl font-semibold text-ink-strong">
              Billable items
            </h1>
            <FilterTabs
              current={filter.value}
              onChange={(param) =>
                setSearchParams(param === "all" ? {} : { filter: param }, {
                  replace: true,
                })
              }
            />
          </div>
          <p className="-mt-2 text-sm text-ink-muted">
            Add and edit items from each project&apos;s page.
          </p>
          <RelayErrorBoundary canRetry>
            <Suspense fallback={<LoadingIndicator />}>
              <Content
                key={instance.id}
                instanceId={instance.id}
                slug={instance.slug}
                filter={filter}
              />
            </Suspense>
          </RelayErrorBoundary>
        </div>
      )}
    </RequireFinanceInstance>
  );
}
