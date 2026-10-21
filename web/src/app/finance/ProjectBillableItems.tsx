import { Suspense, useCallback, useMemo, useState } from "react";
import { graphql, useMutation } from "react-relay";
import { useNavigate } from "react-router";
import type { ProjectBillableItemsQuery } from "./__generated__/ProjectBillableItemsQuery.graphql";
import type { ProjectBillableItemsCreateMutation } from "./__generated__/ProjectBillableItemsCreateMutation.graphql";
import type { ProjectBillableItemsCreateInvoiceMutation } from "./__generated__/ProjectBillableItemsCreateInvoiceMutation.graphql";
import { useRetryableLazyLoadQuery } from "../../components/useRetryableLazyLoadQuery";
import RelayErrorBoundary from "../../components/RelayErrorBoundary";
import LoadingIndicator from "../../components/LoadingIndicator";
import { Card } from "../../components/ui/Card";
import { Button } from "../../components/ui/Button";
import { relayMutationErrorMessage } from "../../lib/relayMutationError";
import { localToday } from "../../lib/dates";
import { BillableItemList } from "./BillableItemList";
import { BillableItemForm } from "./BillableItemForm";

const projectBillableItemsQuery = graphql`
  query ProjectBillableItemsQuery($instanceId: ID!, $projectId: ID!)
  @throwOnFieldError {
    ...BillableItemList_query
      @arguments(instanceId: $instanceId, projectId: $projectId, filter: ALL)
  }
`;

function Items({
  instanceId,
  projectId,
  currency,
  refreshKey,
  selectedIds,
  onToggleSelect,
  onItemDeleted,
  onVisibleUnbilledIdsChange,
}: {
  instanceId: string;
  projectId: string;
  currency: string;
  refreshKey: number;
  selectedIds: ReadonlySet<string>;
  onToggleSelect: (id: string) => void;
  onItemDeleted: (id: string) => void;
  onVisibleUnbilledIdsChange: (ids: readonly string[]) => void;
}) {
  const data = useRetryableLazyLoadQuery<ProjectBillableItemsQuery>(
    projectBillableItemsQuery,
    { instanceId, projectId },
  );
  return (
    <BillableItemList
      query={data}
      filter="ALL"
      showProject={false}
      editable
      currency={currency}
      emptyMessage="No billable items on this project yet."
      refreshKey={refreshKey}
      selectable
      selectedIds={selectedIds}
      onToggleSelect={onToggleSelect}
      onItemDeleted={onItemDeleted}
      onVisibleUnbilledIdsChange={onVisibleUnbilledIdsChange}
    />
  );
}

/**
 * "Add item" for one project. After a successful create the form remounts
 * (fresh fields) but keeps the date just used — entering a day's work item
 * by item shouldn't mean re-picking the date each time — and tells the list
 * to refetch, so the new item appears in its date-ordered place.
 */
export function AddBillableItemForm({
  projectId,
  currency,
  defaultPrice = "",
  onCreated,
}: {
  projectId: string;
  currency: string;
  /** The project's default rate, pre-filled into each new item. */
  defaultPrice?: string;
  onCreated: () => void;
}) {
  const [formKey, setFormKey] = useState(0);
  const [lastDate, setLastDate] = useState(() => localToday());
  const [error, setError] = useState<string | null>(null);

  const [commit, isSaving] = useMutation<ProjectBillableItemsCreateMutation>(
    graphql`
      mutation ProjectBillableItemsCreateMutation(
        $projectId: ID!
        $input: BillableItemInput!
      ) {
        createBillableItem(projectId: $projectId, input: $input) {
          id
        }
      }
    `,
  );

  return (
    <Card>
      <h3 className="mb-4 text-sm font-semibold tracking-wide text-ink-muted uppercase">
        Add item
      </h3>
      <BillableItemForm
        key={formKey}
        idPrefix="new-item"
        currency={currency}
        initial={{
          date: lastDate,
          description: "",
          quantity: "",
          price: defaultPrice,
        }}
        submitLabel="Add item"
        savingLabel="Adding…"
        isSaving={isSaving}
        error={error}
        onSubmit={(input) => {
          setError(null);
          commit({
            variables: { projectId, input },
            onCompleted: () => {
              setLastDate(input.date);
              setFormKey((k) => k + 1);
              onCreated();
            },
            onError: (err) =>
              setError(relayMutationErrorMessage(err, "Failed to add item.")),
          });
        }}
      />
    </Card>
  );
}

/**
 * "Create invoice from selected" — enabled once at least one unbilled item
 * is checked in {@link Items} above. Navigates to the new draft on success,
 * so there's nothing to refresh locally: the invoice detail page fetches
 * fresh, and any store copy of the selected items updates in place because
 * the mutation's response selects the same `BillableItemRow_item` shape
 * they're already cached under (their `status`/`invoice` flip to `DRAFT`
 * without a manual store edit).
 */
function CreateInvoiceButton({
  projectId,
  selectedIds,
}: {
  projectId: string;
  selectedIds: ReadonlySet<string>;
}) {
  const navigate = useNavigate();
  const [error, setError] = useState<string | null>(null);
  const [commit, isCreating] =
    useMutation<ProjectBillableItemsCreateInvoiceMutation>(graphql`
      mutation ProjectBillableItemsCreateInvoiceMutation(
        $projectId: ID!
        $itemIds: [ID!]!
      ) {
        createInvoice(projectId: $projectId, itemIds: $itemIds) {
          id
          items {
            id
            ...BillableItemRow_item
          }
        }
      }
    `);

  return (
    <div className="flex flex-col gap-2">
      <Button
        disabled={selectedIds.size === 0 || isCreating}
        onClick={() => {
          setError(null);
          commit({
            variables: { projectId, itemIds: Array.from(selectedIds) },
            onCompleted: (data) => {
              navigate(`/app/invoices/${data.createInvoice.id}`);
            },
            onError: (err) =>
              setError(
                relayMutationErrorMessage(err, "Failed to create invoice."),
              ),
          });
        }}
      >
        {isCreating
          ? "Creating…"
          : `Create invoice from selected (${selectedIds.size})`}
      </Button>
      {error && (
        <p role="alert" className="text-sm text-red-600 dark:text-red-400">
          {error}
        </p>
      )}
    </div>
  );
}

/**
 * The project page's "Billable items" section: this project's items (with
 * inline Edit/Delete for unbilled/draft ones, checkboxes on unbilled ones,
 * and a "Create invoice from selected" action once something's checked)
 * and, unless the project is archived, the "Add item" form. `instanceId` is
 * the *project's* instance, not the switcher's selection — `billableItems`
 * rejects a project from any other instance, and a deep link can arrive
 * with a different one selected.
 */
export function ProjectBillableItems({
  instanceId,
  projectId,
  currency,
  archived,
  defaultPrice = "",
}: {
  instanceId: string;
  projectId: string;
  currency: string;
  archived: boolean;
  /** The project's default rate as typed, or "" for none. */
  defaultPrice?: string;
}) {
  const [refreshKey, setRefreshKey] = useState(0);
  const [selectedIds, setSelectedIds] = useState<Set<string>>(new Set());
  // The ids `Items` currently renders with an unbilled checkbox — reported
  // up by `BillableItemList`. Deleting a selected item already prunes it
  // from `selectedIds` directly (via `onItemDeleted` below); this is a
  // second, defensive line against the same class of bug for any other way
  // a selected id could stop being a checkable unbilled item.
  const [visibleUnbilledIds, setVisibleUnbilledIds] = useState<
    ReadonlySet<string>
  >(() => new Set());

  const toggleSelect = useCallback((id: string) => {
    setSelectedIds((prev) => {
      const next = new Set(prev);
      if (next.has(id)) {
        next.delete(id);
      } else {
        next.add(id);
      }
      return next;
    });
  }, []);

  const handleItemDeleted = useCallback((id: string) => {
    setSelectedIds((prev) => {
      if (!prev.has(id)) return prev;
      const next = new Set(prev);
      next.delete(id);
      return next;
    });
  }, []);

  const handleVisibleUnbilledIdsChange = useCallback(
    (ids: readonly string[]) => setVisibleUnbilledIds(new Set(ids)),
    [],
  );

  const effectiveSelectedIds = useMemo(
    () =>
      new Set(
        Array.from(selectedIds).filter((id) => visibleUnbilledIds.has(id)),
      ),
    [selectedIds, visibleUnbilledIds],
  );

  return (
    <section
      aria-labelledby="billable-items-heading"
      className="flex flex-col gap-4"
    >
      <h2
        id="billable-items-heading"
        className="text-lg font-semibold text-ink-strong"
      >
        Billable items
      </h2>
      <RelayErrorBoundary canRetry>
        <Suspense fallback={<LoadingIndicator />}>
          <Items
            instanceId={instanceId}
            projectId={projectId}
            currency={currency}
            refreshKey={refreshKey}
            selectedIds={selectedIds}
            onToggleSelect={toggleSelect}
            onItemDeleted={handleItemDeleted}
            onVisibleUnbilledIdsChange={handleVisibleUnbilledIdsChange}
          />
        </Suspense>
      </RelayErrorBoundary>
      {effectiveSelectedIds.size > 0 && (
        <CreateInvoiceButton
          projectId={projectId}
          selectedIds={effectiveSelectedIds}
        />
      )}
      {archived ? (
        <p className="rounded-lg border border-dashed border-line p-4 text-sm text-ink-muted">
          This project is archived, so it can&apos;t take new items. Untick
          &quot;Archived&quot; above to add more.
        </p>
      ) : (
        <div className="max-w-3xl">
          <AddBillableItemForm
            projectId={projectId}
            currency={currency}
            defaultPrice={defaultPrice}
            onCreated={() => setRefreshKey((k) => k + 1)}
          />
        </div>
      )}
    </section>
  );
}
