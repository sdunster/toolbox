import { useState } from "react";
import { graphql, useMutation } from "react-relay";
import type { AddExpenseFormMutation } from "./__generated__/AddExpenseFormMutation.graphql";
import { Card } from "../../components/ui/Card";
import { relayMutationErrorMessage } from "../../lib/relayMutationError";
import { localToday } from "../../lib/dates";
import { ExpenseForm } from "./ExpenseForm";
import { blankExpense } from "./expenseFormDefaults";

/**
 * "Add expense". After a successful create the form remounts with fresh
 * fields but keeps the date and category just used — logging a run of
 * receipts or trips shouldn't mean re-picking them each time.
 *
 * `projectId` fixes the project (a project's own page); otherwise
 * `projects` feeds the picker and the expense starts with no project.
 */
export function AddExpenseForm({
  instanceId,
  currency,
  projectId = null,
  projects,
  onCreated,
}: {
  instanceId: string;
  currency: string;
  projectId?: string | null;
  projects?: ReadonlyArray<{ readonly id: string; readonly name: string }>;
  onCreated: () => void;
}) {
  const [formKey, setFormKey] = useState(0);
  const [last, setLast] = useState(() => blankExpense(localToday(), projectId));
  const [error, setError] = useState<string | null>(null);

  const [commit, isSaving] = useMutation<AddExpenseFormMutation>(graphql`
    mutation AddExpenseFormMutation($instanceId: ID!, $input: ExpenseInput!) {
      createExpense(instanceId: $instanceId, input: $input) {
        id
      }
    }
  `);

  return (
    <Card>
      <h3 className="mb-4 text-sm font-semibold tracking-wide text-ink-muted uppercase">
        Add expense
      </h3>
      <ExpenseForm
        key={formKey}
        idPrefix="new-expense"
        currency={currency}
        initial={last}
        projects={projects}
        submitLabel="Add expense"
        savingLabel="Adding…"
        isSaving={isSaving}
        error={error}
        onSubmit={(input) => {
          setError(null);
          commit({
            variables: { instanceId, input },
            onCompleted: () => {
              setLast({
                ...blankExpense(input.date, input.projectId),
                category: input.category,
              });
              setFormKey((k) => k + 1);
              onCreated();
            },
            onError: (err) =>
              setError(
                relayMutationErrorMessage(err, "Failed to add expense."),
              ),
          });
        }}
      />
    </Card>
  );
}
