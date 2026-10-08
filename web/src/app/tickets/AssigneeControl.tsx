import { useState } from "react";
import { graphql, useFragment, useMutation } from "react-relay";
import type { AssigneeControl_ticket$key } from "./__generated__/AssigneeControl_ticket.graphql";
import type { AssigneeControlMutation } from "./__generated__/AssigneeControlMutation.graphql";

const assigneeControlFragment = graphql`
  fragment AssigneeControl_ticket on Ticket {
    id
    assigneeUserId
    instance {
      id
      members {
        user {
          id
          name
          email
        }
      }
    }
  }
`;

/**
 * Who owns this ticket: a picker over every member of the ticket's instance.
 * `Instance.members` is readable by any member (see its doc comment), so
 * owners and agents alike can hand a ticket to a colleague.
 */
export function AssigneeControl({
  ticket,
}: {
  ticket: AssigneeControl_ticket$key;
}) {
  const data = useFragment(assigneeControlFragment, ticket);
  const [error, setError] = useState<string | null>(null);
  const [commit, isInFlight] = useMutation<AssigneeControlMutation>(graphql`
    mutation AssigneeControlMutation($ticketId: ID!, $userId: ID) {
      assignTicket(ticketId: $ticketId, userId: $userId) {
        id
        assigneeUserId
        assignee {
          id
          name
          email
        }
      }
    }
  `);

  function assignTo(userId: string | null, name: string | null) {
    setError(null);
    commit({
      variables: { ticketId: data.id, userId },
      optimisticResponse: {
        assignTicket: {
          id: data.id,
          assigneeUserId: userId,
          assignee: userId ? { id: userId, name: name ?? "", email: "" } : null,
        },
      },
      onError: () => setError("Failed to update assignment."),
    });
  }

  const members = data.instance.members;

  return (
    <div className="flex flex-col items-end gap-1.5">
      <div className="flex items-center gap-2 text-sm">
        <span className="text-ink-muted">Assignee:</span>
        <select
          aria-label="Assignee"
          className="rounded-md border border-line bg-surface px-2 py-1 text-sm text-ink"
          value={data.assigneeUserId ?? ""}
          disabled={isInFlight}
          onChange={(e) => {
            const userId = e.target.value || null;
            const member = members.find((m) => m.user.id === userId);
            assignTo(userId, member?.user.name ?? null);
          }}
        >
          <option value="">Unassigned</option>
          {members.map((m) => (
            <option key={m.user.id} value={m.user.id} title={m.user.email}>
              {m.user.name}
            </option>
          ))}
        </select>
      </div>
      {error && (
        <p role="alert" className="text-sm text-red-600 dark:text-red-400">
          {error}
        </p>
      )}
    </div>
  );
}
