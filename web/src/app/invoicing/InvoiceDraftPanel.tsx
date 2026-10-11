import { Suspense, useState } from "react";
import { graphql, useFragment, useMutation } from "react-relay";
import { Link, useNavigate } from "react-router";
import type { InvoiceDraftPanel_invoice$key } from "./__generated__/InvoiceDraftPanel_invoice.graphql";
import type { InvoiceDraftPanelRemoveMutation } from "./__generated__/InvoiceDraftPanelRemoveMutation.graphql";
import type { InvoiceDraftPanelDeleteMutation } from "./__generated__/InvoiceDraftPanelDeleteMutation.graphql";
import type { InvoiceDraftPanelFinalizeMutation } from "./__generated__/InvoiceDraftPanelFinalizeMutation.graphql";
import type { InvoiceDraftPanelAddItemsQuery } from "./__generated__/InvoiceDraftPanelAddItemsQuery.graphql";
import type { InvoiceDraftPanelAddItemsMutation } from "./__generated__/InvoiceDraftPanelAddItemsMutation.graphql";
import { useRetryableLazyLoadQuery } from "../../components/useRetryableLazyLoadQuery";
import RelayErrorBoundary from "../../components/RelayErrorBoundary";
import LoadingIndicator from "../../components/LoadingIndicator";
import { Button } from "../../components/ui/Button";
import { FormField } from "../../components/ui/FormField";
import TextInput from "../../components/ui/TextInput";
import { relayMutationErrorMessage } from "../../lib/relayMutationError";
import { addDays, formatDate, localToday } from "../../lib/dates";
import { formatCents } from "../../lib/money";
import { BillableItemRow } from "./BillableItemRow";

// `billableItems` is a connection; a project could in principle have more
// unbilled items than this, but an invoice takes at most 50 anyway
// (`MAX_INVOICE_ITEMS`) — generous enough to show "everything eligible"
// without building a second pagination UI just for this panel.
const ADD_ITEMS_LIMIT = 100;

const invoiceDraftPanelFragment = graphql`
  fragment InvoiceDraftPanel_invoice on Invoice {
    id
    currency
    project {
      id
      effectivePaymentTermsDays
    }
    items {
      id
      ...BillableItemRow_item
    }
  }
`;

function AddInvoiceItemsPanel({
  invoiceId,
  instanceId,
  projectId,
  onAdded,
}: {
  invoiceId: string;
  instanceId: string;
  projectId: string;
  onAdded: () => void;
}) {
  const data = useRetryableLazyLoadQuery<InvoiceDraftPanelAddItemsQuery>(
    graphql`
      query InvoiceDraftPanelAddItemsQuery(
        $instanceId: ID!
        $projectId: ID!
        $first: Int!
      ) @throwOnFieldError {
        billableItems(
          instanceId: $instanceId
          projectId: $projectId
          filter: UNBILLED
          first: $first
        ) {
          edges {
            node {
              id
              date
              description
              quantity
              unitPriceCents
              amountCents
            }
          }
        }
      }
    `,
    { instanceId, projectId, first: ADD_ITEMS_LIMIT },
    // Always hit the network: this panel mounts each time it's opened (see
    // `InvoiceDraftPanel`'s `showAdd` toggle below), and a store-or-network
    // read could otherwise serve a cached page that still lists an item
    // that's since been added to (or removed from) this very invoice —
    // adding it again would then fail `CONFLICT` until an unrelated reload.
    { fetchPolicy: "network-only" },
  );
  const [selected, setSelected] = useState<Set<string>>(new Set());
  const [error, setError] = useState<string | null>(null);
  const [commit, isAdding] = useMutation<InvoiceDraftPanelAddItemsMutation>(
    graphql`
      mutation InvoiceDraftPanelAddItemsMutation(
        $invoiceId: ID!
        $itemIds: [ID!]!
      ) {
        addInvoiceItems(invoiceId: $invoiceId, itemIds: $itemIds) {
          id
          items {
            id
            ...BillableItemRow_item
          }
          ...InvoicePreview_invoice
        }
      }
    `,
  );

  const nodes = data.billableItems.edges.map((edge) => edge.node);

  function toggle(id: string) {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(id)) {
        next.delete(id);
      } else {
        next.add(id);
      }
      return next;
    });
  }

  if (nodes.length === 0) {
    return (
      <p className="rounded-lg border border-dashed border-line p-4 text-sm text-ink-muted">
        No unbilled items on this project.
      </p>
    );
  }

  return (
    <div className="flex flex-col gap-3 rounded-lg border border-line p-4">
      <ul className="flex flex-col divide-y divide-line-faint">
        {nodes.map((item) => (
          <li key={item.id} className="flex items-start gap-3 py-2 text-sm">
            <input
              type="checkbox"
              className="mt-1 size-4 rounded-sm border-line text-accent focus:ring-2 focus:ring-accent/25"
              checked={selected.has(item.id)}
              onChange={() => toggle(item.id)}
              aria-label={`Select ${item.description.split("\n")[0]}`}
            />
            <div className="flex flex-1 flex-wrap items-baseline justify-between gap-2">
              <div className="min-w-0">
                <span className="text-ink-muted tabular-nums">
                  {formatDate(item.date)}
                </span>{" "}
                <span className="text-ink-strong">
                  {item.description.split("\n")[0]}
                </span>
              </div>
              <span className="text-ink-muted tabular-nums">
                {item.quantity} × {formatCents(item.unitPriceCents)} ={" "}
                {formatCents(item.amountCents)}
              </span>
            </div>
          </li>
        ))}
      </ul>
      {error && (
        <p role="alert" className="text-sm text-red-600 dark:text-red-400">
          {error}
        </p>
      )}
      <Button
        disabled={isAdding || selected.size === 0}
        className="self-start"
        onClick={() => {
          setError(null);
          commit({
            variables: { invoiceId, itemIds: Array.from(selected) },
            onCompleted: () => onAdded(),
            onError: (err) =>
              setError(relayMutationErrorMessage(err, "Failed to add items.")),
          });
        }}
      >
        {isAdding ? "Adding…" : "Add selected items"}
      </Button>
    </div>
  );
}

/**
 * A draft invoice's own actions: manage its items (remove one, or open
 * {@link AddInvoiceItemsPanel} to add more of the project's unbilled
 * ones), delete the draft outright, or finalize it. `isOwner` decides
 * whether a "Complete the invoicing settings first" error (from
 * `finalizeInvoice`, when the instance has no business name set yet) links
 * to Business settings — only an owner can do anything about it there.
 */
export function InvoiceDraftPanel({
  invoice,
  instanceId,
  isOwner,
}: {
  invoice: InvoiceDraftPanel_invoice$key;
  instanceId: string;
  isOwner: boolean;
}) {
  const data = useFragment(invoiceDraftPanelFragment, invoice);
  const navigate = useNavigate();
  const [showAdd, setShowAdd] = useState(false);
  const [showFinalize, setShowFinalize] = useState(false);
  const [confirmingDelete, setConfirmingDelete] = useState(false);
  const [removingId, setRemovingId] = useState<string | null>(null);
  const [issueDate, setIssueDate] = useState(() => localToday());
  // `null` until the user picks one: the default follows the issue date.
  const [dueDateChoice, setDueDateChoice] = useState<string | null>(null);
  const defaultDueDate = addDays(
    issueDate,
    data.project.effectivePaymentTermsDays,
  );
  const dueDate = dueDateChoice ?? defaultDueDate;
  const [error, setError] = useState<string | null>(null);

  const [commitRemove] = useMutation<InvoiceDraftPanelRemoveMutation>(graphql`
    mutation InvoiceDraftPanelRemoveMutation(
      $invoiceId: ID!
      $itemIds: [ID!]!
    ) {
      removeInvoiceItems(invoiceId: $invoiceId, itemIds: $itemIds) {
        id
        items {
          id
          ...BillableItemRow_item
        }
        ...InvoicePreview_invoice
      }
    }
  `);
  const [commitDelete, isDeleting] =
    useMutation<InvoiceDraftPanelDeleteMutation>(graphql`
      mutation InvoiceDraftPanelDeleteMutation($invoiceId: ID!) {
        deleteInvoice(invoiceId: $invoiceId)
      }
    `);
  const [commitFinalize, isFinalizing] =
    useMutation<InvoiceDraftPanelFinalizeMutation>(graphql`
      mutation InvoiceDraftPanelFinalizeMutation(
        $invoiceId: ID!
        $issueDate: String!
        $dueDate: String
      ) {
        finalizeInvoice(
          invoiceId: $invoiceId
          issueDate: $issueDate
          dueDate: $dueDate
        ) {
          id
          status
          items {
            id
            ...BillableItemRow_item
          }
          ...InvoicePreview_invoice
          ...InvoicePaidControl_invoice
          ...InvoiceSendPanel_invoice
          ...InvoiceCreditNotes_invoice
        }
      }
    `);

  function handleRemove(itemId: string) {
    setError(null);
    setRemovingId(itemId);
    commitRemove({
      variables: { invoiceId: data.id, itemIds: [itemId] },
      // A removed item drops out of the returned invoice's own `items`,
      // so nothing there updates its now-stale `status`/`invoice` — patch
      // that record directly rather than leaving it looking DRAFT
      // wherever else it's cached (e.g. the project page).
      updater: (store) => {
        const record = store.get(itemId);
        if (record) {
          record.setValue("UNBILLED", "status");
          record.setLinkedRecord(null, "invoice");
        }
      },
      onCompleted: () => setRemovingId(null),
      onError: (err) => {
        setRemovingId(null);
        setError(relayMutationErrorMessage(err, "Failed to remove item."));
      },
    });
  }

  function handleDelete() {
    setError(null);
    const itemIds = data.items.map((item) => item.id);
    commitDelete({
      variables: { invoiceId: data.id },
      updater: (store) => {
        for (const itemId of itemIds) {
          const record = store.get(itemId);
          if (record) {
            record.setValue("UNBILLED", "status");
            record.setLinkedRecord(null, "invoice");
          }
        }
        // The invoice list(s) this draft appeared in aren't reachable from
        // here (no connection id) — invalidate so the next store-or-network
        // read of them (the /app/invoices page we're about to land on)
        // fetches fresh instead of serving a stale cached copy.
        store.invalidateStore();
      },
      onCompleted: () => {
        navigate("/app/invoices");
      },
      onError: (err) => {
        setConfirmingDelete(false);
        setError(relayMutationErrorMessage(err, "Failed to delete draft."));
      },
    });
  }

  function handleFinalize(e: React.FormEvent) {
    e.preventDefault();
    setError(null);
    commitFinalize({
      variables: { invoiceId: data.id, issueDate, dueDate },
      onCompleted: () => setShowFinalize(false),
      onError: (err) =>
        setError(relayMutationErrorMessage(err, "Failed to finalize invoice.")),
    });
  }

  const businessSettingsHint =
    error !== null && /invoicing settings/i.test(error);

  return (
    <div className="flex flex-col gap-4">
      <section className="flex flex-col gap-3">
        <h2 className="text-sm font-semibold tracking-wide text-ink-muted uppercase">
          Items on this invoice
        </h2>
        {data.items.length === 0 ? (
          <p className="rounded-lg border border-dashed border-line p-4 text-sm text-ink-muted">
            No items yet — add some below.
          </p>
        ) : (
          <div className="rounded-lg border border-line">
            <ul className="flex flex-col divide-y divide-line-faint">
              {data.items.map((item) => (
                <li key={item.id}>
                  <BillableItemRow
                    item={item}
                    showProject={false}
                    editable={false}
                    currency={data.currency}
                    connectionId=""
                    onChanged={() => {}}
                    onRemove={handleRemove}
                    removing={removingId === item.id}
                  />
                </li>
              ))}
            </ul>
          </div>
        )}
      </section>

      <div className="flex flex-wrap gap-2">
        <Button variant="secondary" onClick={() => setShowAdd((v) => !v)}>
          {showAdd ? "Hide add items" : "Add items"}
        </Button>
        {!confirmingDelete ? (
          <Button
            variant="secondary"
            className="text-red-700 dark:text-red-400"
            onClick={() => setConfirmingDelete(true)}
          >
            Delete draft
          </Button>
        ) : (
          <>
            <Button
              variant="secondary"
              className="bg-red-700 text-white hover:bg-red-600 dark:bg-red-600"
              disabled={isDeleting}
              onClick={handleDelete}
            >
              {isDeleting ? "Deleting…" : "Confirm delete"}
            </Button>
            <Button
              variant="ghost"
              disabled={isDeleting}
              onClick={() => setConfirmingDelete(false)}
            >
              Cancel
            </Button>
          </>
        )}
        <Button
          onClick={() => setShowFinalize((v) => !v)}
          disabled={data.items.length === 0}
        >
          Finalize
        </Button>
      </div>

      {showAdd && (
        <RelayErrorBoundary canRetry>
          <Suspense fallback={<LoadingIndicator />}>
            <AddInvoiceItemsPanel
              invoiceId={data.id}
              instanceId={instanceId}
              projectId={data.project.id}
              onAdded={() => setShowAdd(false)}
            />
          </Suspense>
        </RelayErrorBoundary>
      )}

      {showFinalize && (
        <div className="rounded-lg border border-line bg-surface-raised p-4">
          <form onSubmit={handleFinalize} className="flex flex-col gap-3">
            <div className="w-48">
              <FormField label="Issue date" htmlFor="finalize-issue-date">
                <TextInput
                  id="finalize-issue-date"
                  type="date"
                  value={issueDate}
                  onChange={(e) => setIssueDate(e.target.value)}
                  required
                />
              </FormField>
            </div>
            <div className="w-48">
              <FormField label="Due date" htmlFor="finalize-due-date">
                <TextInput
                  id="finalize-due-date"
                  type="date"
                  value={dueDate}
                  min={issueDate}
                  onChange={(e) => setDueDateChoice(e.target.value || null)}
                  required
                />
              </FormField>
              <p className="mt-1 text-xs text-ink-muted">
                {data.project.effectivePaymentTermsDays} days&apos; terms by
                default.
              </p>
            </div>
            <p className="text-sm text-amber-700 dark:text-amber-400">
              Finalizing assigns the next invoice number and makes this invoice
              read-only. This cannot be undone.
            </p>
            <div className="flex gap-2">
              <Button type="submit" disabled={isFinalizing}>
                {isFinalizing ? "Finalizing…" : "Finalize"}
              </Button>
              <Button
                type="button"
                variant="secondary"
                disabled={isFinalizing}
                onClick={() => setShowFinalize(false)}
              >
                Cancel
              </Button>
            </div>
          </form>
        </div>
      )}

      {error && (
        <p role="alert" className="text-sm text-red-600 dark:text-red-400">
          {error}
          {businessSettingsHint && isOwner && (
            <>
              {" "}
              An owner can complete them in{" "}
              <Link to="/app/invoicing-settings" className="underline">
                Business settings
              </Link>
              .
            </>
          )}
        </p>
      )}
    </div>
  );
}
