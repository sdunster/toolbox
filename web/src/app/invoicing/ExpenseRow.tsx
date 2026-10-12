import { useState } from "react";
import { graphql, useFragment, useMutation } from "react-relay";
import { Link } from "react-router";
import type { ExpenseRow_expense$key } from "./__generated__/ExpenseRow_expense.graphql";
import type { ExpenseRowUpdateMutation } from "./__generated__/ExpenseRowUpdateMutation.graphql";
import type { ExpenseRowDeleteMutation } from "./__generated__/ExpenseRowDeleteMutation.graphql";
import { relayMutationErrorMessage } from "../../lib/relayMutationError";
import { formatDate } from "../../lib/dates";
import { tw } from "../../lib/tw";
import { centsToInput, formatCents } from "../../lib/money";
import { VEHICLE_KM, categoryLabel } from "../../lib/expenses";
import { ExpenseForm } from "./ExpenseForm";
import { ExpenseExtras } from "./ExpenseExtras";

const expenseRowFragment = graphql`
  fragment ExpenseRow_expense on Expense {
    id
    date
    category
    description
    supplier
    amountCents
    gstCents
    distanceKm
    rateCentsPerKm
    project {
      id
      name
    }
    rebilledItem {
      id
    }
    receipt {
      filename
    }
  }
`;

/** Compact text-style action buttons, matching `BillableItemRow`. */
const actionButton = tw`cursor-pointer rounded px-2 py-1 text-sm font-medium focus-visible:ring-2 focus-visible:ring-accent/50 focus-visible:outline-none`;

/**
 * The lg+ column templates, shared by {@link ExpenseHeader} and every row:
 * date, (project), category, details, amount, actions.
 */
const gridCols = {
  withProject: tw`lg:grid-cols-[6rem_minmax(0,10rem)_minmax(0,11rem)_minmax(0,1fr)_8rem_9rem]`,
  withoutProject: tw`lg:grid-cols-[6rem_minmax(0,11rem)_minmax(0,1fr)_8rem_9rem]`,
};

export function ExpenseHeader({
  showProject,
  currency,
}: {
  showProject: boolean;
  currency: string;
}) {
  return (
    <div
      className={`hidden gap-x-4 border-b border-line px-3 py-2 text-xs font-medium tracking-wide text-ink-muted uppercase lg:grid ${showProject ? gridCols.withProject : gridCols.withoutProject}`}
      aria-hidden="true"
    >
      <span>Date</span>
      {showProject && <span>Project</span>}
      <span>Category</span>
      <span>Details</span>
      <span className="text-right">Amount ({currency})</span>
      <span />
    </div>
  );
}

/**
 * One expense: a grid row at lg+, a stacked card below (the same
 * `lg:contents` technique as `BillableItemRow`). Inline Edit swaps in an
 * {@link ExpenseForm}; Delete asks for a second click. Below the details,
 * {@link ExpenseExtras} handles the receipt and re-billing.
 *
 * `projects` feeds the edit form's project picker; on a project's own page
 * it's omitted and the expense keeps its project.
 */
export function ExpenseRow({
  expense,
  showProject,
  currency,
  projects,
  connectionId,
  onChanged,
}: {
  expense: ExpenseRow_expense$key;
  showProject: boolean;
  currency: string;
  projects?: ReadonlyArray<{ readonly id: string; readonly name: string }>;
  connectionId: string;
  onChanged: () => void;
}) {
  const data = useFragment(expenseRowFragment, expense);
  const [editing, setEditing] = useState(false);
  const [confirmingDelete, setConfirmingDelete] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const [commitUpdate, isUpdating] = useMutation<ExpenseRowUpdateMutation>(
    graphql`
      mutation ExpenseRowUpdateMutation($id: ID!, $input: ExpenseInput!) {
        updateExpense(id: $id, input: $input) {
          ...ExpenseRow_expense
        }
      }
    `,
  );
  const [commitDelete, isDeleting] = useMutation<ExpenseRowDeleteMutation>(
    graphql`
      mutation ExpenseRowDeleteMutation($id: ID!, $connections: [ID!]!) {
        deleteExpense(id: $id) @deleteEdge(connections: $connections)
      }
    `,
  );

  const isTrip = data.category === VEHICLE_KM;

  if (editing) {
    return (
      <div className="bg-surface-raised p-3 sm:p-4">
        <h3 className="mb-3 text-sm font-semibold text-ink-strong">
          Edit expense
        </h3>
        <ExpenseForm
          idPrefix={`edit-${data.id}`}
          currency={currency}
          projects={projects}
          initial={{
            projectId: data.project?.id ?? null,
            date: data.date,
            category: data.category as never,
            description: data.description ?? "",
            supplier: data.supplier ?? "",
            amount: isTrip ? "" : centsToInput(data.amountCents),
            gst: isTrip
              ? undefined
              : data.gstCents == null
                ? null
                : centsToInput(data.gstCents),
            distanceKm: data.distanceKm ?? "",
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
                  relayMutationErrorMessage(err, "Failed to save expense."),
                ),
            });
          }}
        />
      </div>
    );
  }

  return (
    <div
      className={`flex flex-col gap-1.5 p-3 lg:grid lg:items-start lg:gap-x-4 ${showProject ? gridCols.withProject : gridCols.withoutProject}`}
    >
      <div className="flex min-w-0 flex-wrap items-baseline gap-x-2 lg:contents">
        <span className="text-sm text-ink tabular-nums">
          {formatDate(data.date)}
        </span>
        {showProject &&
          (data.project ? (
            <Link
              to={`/app/projects/${data.project.id}`}
              className="min-w-0 truncate text-sm text-accent hover:underline"
              title={data.project.name}
            >
              {data.project.name}
            </Link>
          ) : (
            <span className="hidden text-sm text-ink-muted lg:inline">—</span>
          ))}
        <span className="text-sm text-ink-muted">
          {categoryLabel(data.category)}
        </span>
      </div>

      <div className="min-w-0 text-sm text-ink-strong">
        {isTrip ? (
          <>
            <p className="wrap-break-word">{data.description}</p>
            <p className="text-ink-muted tabular-nums">
              {data.distanceKm} km × {data.rateCentsPerKm}c
            </p>
          </>
        ) : (
          <>
            <p className="wrap-break-word">{data.supplier}</p>
            {data.description && (
              <p className="wrap-break-word text-ink-muted">
                {data.description}
              </p>
            )}
          </>
        )}
        <ExpenseExtras
          expenseId={data.id}
          hasProject={data.project != null}
          rebilled={data.rebilledItem != null}
          receiptFilename={data.receipt?.filename ?? null}
          onChanged={onChanged}
        />
      </div>

      <div className="text-sm tabular-nums lg:text-right">
        <span className="font-medium text-ink-strong">
          <span className="sr-only">Amount </span>
          {formatCents(data.amountCents)}
          <span className="text-ink-muted lg:hidden"> {currency}</span>
        </span>
        <p className="text-xs text-ink-muted">
          {isTrip
            ? "No GST"
            : data.gstCents == null
              ? "GST-free"
              : `incl. ${formatCents(data.gstCents)} GST`}
        </p>
      </div>

      <div className="flex flex-wrap items-center gap-1 lg:-my-1 lg:justify-end">
        {confirmingDelete ? (
          <>
            <button
              type="button"
              className={`${actionButton} bg-red-700 text-white hover:bg-red-600 disabled:opacity-60 dark:bg-red-600`}
              disabled={isDeleting}
              onClick={() => {
                setError(null);
                commitDelete({
                  variables: { id: data.id, connections: [connectionId] },
                  onCompleted: () => onChanged(),
                  onError: (err) => {
                    setConfirmingDelete(false);
                    setError(
                      relayMutationErrorMessage(
                        err,
                        "Failed to delete expense.",
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
        ) : (
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
            <button
              type="button"
              className={`${actionButton} text-red-700 dark:text-red-400`}
              onClick={() => setConfirmingDelete(true)}
            >
              Delete
            </button>
          </>
        )}
      </div>

      {error && (
        <p
          role="alert"
          className="text-sm text-red-600 lg:col-span-full dark:text-red-400"
        >
          {error}
        </p>
      )}
    </div>
  );
}
