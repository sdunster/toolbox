import { useId, useState } from "react";
import { graphql, useFragment, useMutation } from "react-relay";
import { Link } from "react-router";
import type { BillableItemRow_item$key } from "./__generated__/BillableItemRow_item.graphql";
import type { BillableItemRowUpdateMutation } from "./__generated__/BillableItemRowUpdateMutation.graphql";
import type { BillableItemRowDeleteMutation } from "./__generated__/BillableItemRowDeleteMutation.graphql";
import { relayMutationErrorMessage } from "../../lib/relayMutationError";
import { formatDate } from "../../lib/dates";
import { tw } from "../../lib/tw";
import { centsToInput, formatCents } from "../../lib/money";
import { BillableItemForm } from "./BillableItemForm";
import {
  itemGridCols,
  itemStatusBadge,
  itemStatusBadgeBase,
  itemStatusLabel,
} from "./billableItemStyles";

const billableItemRowFragment = graphql`
  fragment BillableItemRow_item on BillableItem {
    id
    date
    description
    quantity
    unitPriceCents
    amountCents
    gstFree
    status
    invoice {
      id
    }
    project {
      id
      name
    }
  }
`;

/** A first line longer than this gets the expand toggle even when it's the
 * only line — the collapsed view clamps it to two lines. */
const LONG_FIRST_LINE = 120;

/** Compact text-style action buttons — the row's Edit/Delete/Confirm. */
const actionButton = tw`cursor-pointer rounded px-2 py-1 text-sm font-medium focus-visible:ring-2 focus-visible:ring-accent/50 focus-visible:outline-none`;

function Description({ text }: { text: string }) {
  const [expanded, setExpanded] = useState(false);
  const bodyId = useId();
  const [firstLine, ...rest] = text.split("\n");
  const expandable = rest.length > 0 || firstLine.length > LONG_FIRST_LINE;

  return (
    <div className="min-w-0 text-sm text-ink-strong">
      <p
        id={bodyId}
        className={
          expanded
            ? "wrap-break-word whitespace-pre-line"
            : "line-clamp-2 wrap-break-word"
        }
      >
        {expanded ? text : firstLine}
      </p>
      {expandable && (
        <button
          type="button"
          className="mt-0.5 cursor-pointer text-xs text-accent hover:underline"
          aria-expanded={expanded}
          aria-controls={bodyId}
          onClick={() => setExpanded((v) => !v)}
        >
          {expanded
            ? "Show less"
            : rest.length > 0
              ? `Show all ${rest.length + 1} lines`
              : "Show more"}
        </button>
      )}
    </div>
  );
}

/**
 * Column headings for a list of {@link BillableItemRow}s — lg+ only; below
 * that each row is a self-describing card.
 */
export function BillableItemHeader({
  showProject,
  editable,
  currency,
}: {
  showProject: boolean;
  editable: boolean;
  currency: string;
}) {
  const cols = showProject
    ? itemGridCols.withProject
    : itemGridCols.withActions;
  return (
    <div
      className={`hidden gap-x-4 border-b border-line px-3 py-2 text-xs font-medium tracking-wide text-ink-muted uppercase lg:grid ${cols}`}
      aria-hidden="true"
    >
      <span>Date</span>
      {showProject && <span>Project</span>}
      <span>Description</span>
      <span className="text-right">Qty</span>
      <span className="text-right">Unit price</span>
      <span className="text-right">Amount ({currency})</span>
      <span>Status</span>
      {editable && <span />}
    </div>
  );
}

/**
 * One billable item. At lg+ a grid row lined up with
 * {@link BillableItemHeader}; below that a stacked card — date, project and
 * status on top, the description, then "qty × unit price" and the amount.
 * The same DOM serves both: the wrappers are `lg:contents`, so at lg+ their
 * children become the grid's cells, placed by `lg:order-*`.
 *
 * With `editable` (a project's own list), an unbilled item gets inline
 * Edit/Delete. Edit swaps the row for a {@link BillableItemForm}; Delete
 * asks for a second click. `onChanged` runs after a successful edit so the
 * list can refetch (a changed date moves the row).
 *
 * Two more, independent affordances share the same trailing cell:
 * - `selectable`: an unbilled row gets a checkbox (the project page's
 *   "create invoice from selected" flow) — `onToggleSelect`/`selectedIds`
 *   drive it, the parent owns the selection; `onDeleted` tells it to drop a
 *   deleted item's id from that selection too.
 * - `onRemove`: renders a "Remove" button instead of Edit/Delete — the
 *   invoice detail page's own items-on-this-invoice list, which never edits
 *   in place, only detaches an item back to unbilled.
 *
 * A `DRAFT`/`INVOICED` item's status badge links to its invoice
 * (`data.invoice`); an `UNBILLED` one, or the data-integrity fallback where
 * `invoice` is somehow missing (see `BillableItem.status`'s doc comment),
 * renders as plain text instead of a dead link.
 */
export function BillableItemRow({
  item,
  showProject,
  editable,
  currency,
  connectionId,
  onChanged,
  selectable = false,
  selectedIds,
  onToggleSelect,
  onRemove,
  removing = false,
  onDeleted,
}: {
  item: BillableItemRow_item$key;
  showProject: boolean;
  editable: boolean;
  currency: string;
  /** The list's connection id, for `@deleteEdge`. */
  connectionId: string;
  onChanged: () => void;
  /** Show a checkbox on unbilled rows for bulk invoice creation. */
  selectable?: boolean;
  selectedIds?: ReadonlySet<string>;
  onToggleSelect?: (id: string) => void;
  /** Present on the invoice detail page's own items list: a "Remove"
   * button in place of Edit/Delete, detaching the item from its invoice. */
  onRemove?: (id: string) => void;
  removing?: boolean;
  /** Called after a successful delete, so a parent tracking a selection
   * (the project page's "create invoice from selected" checkboxes) can drop
   * this id — otherwise it lingers in that selection with no checkbox left
   * to untick, and submitting it fails `NOT_FOUND`. */
  onDeleted?: (id: string) => void;
}) {
  const data = useFragment(billableItemRowFragment, item);
  const [editing, setEditing] = useState(false);
  const [confirmingDelete, setConfirmingDelete] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [commitUpdate, isUpdating] = useMutation<BillableItemRowUpdateMutation>(
    graphql`
      mutation BillableItemRowUpdateMutation(
        $id: ID!
        $input: BillableItemInput!
      ) {
        updateBillableItem(id: $id, input: $input) {
          ...BillableItemRow_item
        }
      }
    `,
  );
  const [commitDelete, isDeleting] = useMutation<BillableItemRowDeleteMutation>(
    graphql`
      mutation BillableItemRowDeleteMutation($id: ID!, $connections: [ID!]!) {
        deleteBillableItem(id: $id) @deleteEdge(connections: $connections)
      }
    `,
  );

  // A draft invoice's items stay editable (the write bumps the invoice's
  // version — see `updateBillableItem`'s doc comment); only unbilled ones
  // may be deleted outright, regardless of the invoice's own status.
  const canEdit =
    editable && (data.status === "UNBILLED" || data.status === "DRAFT");
  const canDelete = editable && data.status === "UNBILLED";

  if (editing) {
    return (
      <div className="bg-surface-raised p-3 sm:p-4">
        <h3 className="mb-3 text-sm font-semibold text-ink-strong">
          Edit item
        </h3>
        <BillableItemForm
          idPrefix={`edit-${data.id}`}
          currency={currency}
          initial={{
            date: data.date,
            description: data.description,
            quantity: data.quantity,
            price: centsToInput(data.unitPriceCents),
            gstFree: data.gstFree,
          }}
          submitLabel="Save"
          savingLabel="Saving…"
          isSaving={isUpdating}
          error={error}
          onCancel={() => {
            setEditing(false);
            setError(null);
          }}
          onSubmit={(input) => {
            setError(null);
            commitUpdate({
              variables: { id: data.id, input },
              onCompleted: () => {
                setEditing(false);
                onChanged();
              },
              onError: (err) =>
                setError(
                  relayMutationErrorMessage(err, "Failed to save item."),
                ),
            });
          }}
        />
      </div>
    );
  }

  const cols = showProject
    ? itemGridCols.withProject
    : itemGridCols.withActions;

  return (
    <div
      className={`flex flex-col gap-1.5 p-3 lg:grid lg:items-start lg:gap-x-4 ${cols}`}
    >
      <div className="flex items-start justify-between gap-3 lg:contents">
        <div className="flex min-w-0 flex-wrap items-baseline gap-x-2 lg:contents">
          <span className="text-sm text-ink tabular-nums lg:order-1">
            {formatDate(data.date)}
          </span>
          {showProject && (
            <Link
              to={`/app/projects/${data.project.id}`}
              className="min-w-0 truncate text-sm text-accent hover:underline lg:order-2"
              title={data.project.name}
            >
              {data.project.name}
            </Link>
          )}
        </div>
        {data.invoice ? (
          <Link
            to={`/app/invoices/${data.invoice.id}`}
            className={`${itemStatusBadgeBase} ${itemStatusBadge[data.status] ?? ""} self-start hover:underline lg:order-7 lg:justify-self-start`}
          >
            {itemStatusLabel[data.status] ?? data.status.toLowerCase()}
          </Link>
        ) : (
          <span
            className={`${itemStatusBadgeBase} ${itemStatusBadge[data.status] ?? ""} self-start lg:order-7 lg:justify-self-start`}
          >
            {itemStatusLabel[data.status] ?? data.status.toLowerCase()}
          </span>
        )}
      </div>

      <div className="min-w-0 lg:order-3">
        <Description text={data.description} />
        {data.gstFree && (
          <span className="mt-0.5 inline-block text-xs text-ink-muted">
            GST-free
          </span>
        )}
      </div>

      <div className="flex items-baseline justify-between gap-3 lg:contents">
        <span className="flex items-baseline gap-1.5 text-sm text-ink-muted tabular-nums lg:contents">
          <span className="lg:order-4 lg:text-right">
            <span className="sr-only">Quantity </span>
            {data.quantity}
          </span>
          <span aria-hidden="true" className="lg:hidden">
            ×
          </span>
          <span className="lg:order-5 lg:text-right">
            <span className="sr-only">Unit price </span>
            {formatCents(data.unitPriceCents)}
          </span>
        </span>
        <span className="text-sm font-medium text-ink-strong tabular-nums lg:order-6 lg:text-right">
          <span className="sr-only">Amount </span>
          {formatCents(data.amountCents)}
          <span className="text-ink-muted lg:hidden"> {currency}</span>
        </span>
      </div>

      {(editable || onRemove) && (
        <div className="flex flex-wrap items-center gap-1 lg:order-8 lg:-my-1 lg:justify-end">
          {selectable && data.status === "UNBILLED" && (
            <label className="mr-1 flex items-center gap-1.5 text-sm text-ink-muted">
              <span className="sr-only">Select for invoice</span>
              <input
                type="checkbox"
                checked={selectedIds?.has(data.id) ?? false}
                onChange={() => onToggleSelect?.(data.id)}
                className="size-4 rounded-sm border-line text-accent focus:ring-2 focus:ring-accent/25"
              />
            </label>
          )}
          {canEdit && !confirmingDelete && (
            <>
              <button
                type="button"
                className={`${actionButton} text-accent`}
                onClick={() => {
                  setError(null);
                  setEditing(true);
                }}
              >
                Edit
              </button>
              {canDelete && (
                <button
                  type="button"
                  className={`${actionButton} text-red-700 dark:text-red-400`}
                  onClick={() => setConfirmingDelete(true)}
                >
                  Delete
                </button>
              )}
            </>
          )}
          {onRemove && (
            <button
              type="button"
              className={`${actionButton} text-red-700 disabled:opacity-60 dark:text-red-400`}
              disabled={removing}
              onClick={() => onRemove(data.id)}
            >
              {removing ? "Removing…" : "Remove"}
            </button>
          )}
          {canDelete && confirmingDelete && (
            <>
              <button
                type="button"
                className={`${actionButton} bg-red-700 text-white hover:bg-red-600 disabled:opacity-60 dark:bg-red-600`}
                disabled={isDeleting}
                onClick={() => {
                  setError(null);
                  commitDelete({
                    variables: { id: data.id, connections: [connectionId] },
                    onCompleted: () => onDeleted?.(data.id),
                    onError: (err) => {
                      setConfirmingDelete(false);
                      setError(
                        relayMutationErrorMessage(
                          err,
                          "Failed to delete item.",
                        ),
                      );
                    },
                  });
                }}
              >
                {isDeleting ? "Deleting…" : "Confirm delete"}
              </button>
              <button
                type="button"
                className={`${actionButton} text-ink-muted`}
                disabled={isDeleting}
                onClick={() => setConfirmingDelete(false)}
              >
                Cancel
              </button>
            </>
          )}
        </div>
      )}

      {error && (
        <p
          role="alert"
          className="text-sm text-red-600 lg:order-9 lg:col-span-full dark:text-red-400"
        >
          {error}
        </p>
      )}
    </div>
  );
}
