import { useState } from "react";
import { graphql, useFragment, useMutation } from "react-relay";
import type { StatusControl_ticket$key } from "./__generated__/StatusControl_ticket.graphql";
import type { StatusControlMutation } from "./__generated__/StatusControlMutation.graphql";
import { Button } from "../../components/ui/Button";
import { statusBadge, statusBadgeBase } from "./ticketStyles";

const statusControlFragment = graphql`
  fragment StatusControl_ticket on Ticket {
    id
    status
  }
`;

/**
 * Close / reopen. A `setTicketStatus` response carries `id` and `status`
 * directly onto the existing `Ticket` record, so Relay merges it in on
 * completion with no custom updater — this is the "explicit record update"
 * half of the build plan's mutation-updater guidance, not
 * `store.invalidateStore()`. The result is fully predictable (we know
 * exactly which status we asked for), so this also uses an optimistic
 * response.
 *
 * "Close quietly" passes `quiet: true`, which skips the closure email the
 * requesters/CCs would otherwise get (staff notifications are unaffected).
 */
export function StatusControl({
  ticket,
}: {
  ticket: StatusControl_ticket$key;
}) {
  const data = useFragment(statusControlFragment, ticket);
  const [error, setError] = useState<string | null>(null);
  const [commit, isInFlight] = useMutation<StatusControlMutation>(graphql`
    mutation StatusControlMutation(
      $ticketId: ID!
      $status: TicketStatusType!
      $quiet: Boolean!
    ) {
      setTicketStatus(ticketId: $ticketId, status: $status, quiet: $quiet) {
        id
        status
        updatedAt
        lastActivityAt
      }
    }
  `);

  function setStatus(status: "OPEN" | "CLOSED", quiet = false) {
    setError(null);
    commit({
      variables: { ticketId: data.id, status, quiet },
      optimisticResponse: {
        setTicketStatus: {
          id: data.id,
          status,
          updatedAt: Math.floor(Date.now() / 1000),
          lastActivityAt: Math.floor(Date.now() / 1000),
        },
      },
      onError: () => setError("Failed to update status."),
    });
  }

  return (
    <div className="flex flex-col items-end gap-1.5">
      <div className="flex items-center gap-2">
        <span
          className={`${statusBadgeBase} ${statusBadge[data.status] ?? ""}`}
        >
          {data.status.toLowerCase()}
        </span>
        {data.status === "OPEN" && (
          <>
            <Button
              variant="ghost"
              disabled={isInFlight}
              title="Close without emailing the requesters and CCs"
              onClick={() => setStatus("CLOSED", true)}
            >
              Close quietly
            </Button>
            <Button
              variant="secondary"
              disabled={isInFlight}
              onClick={() => setStatus("CLOSED")}
            >
              Close
            </Button>
          </>
        )}
        {data.status === "CLOSED" && (
          <Button
            variant="secondary"
            disabled={isInFlight}
            onClick={() => setStatus("OPEN")}
          >
            Reopen
          </Button>
        )}
        {data.status === "DELETED" && (
          <Button
            variant="secondary"
            disabled={isInFlight}
            onClick={() => setStatus("OPEN")}
          >
            Restore
          </Button>
        )}
      </div>
      {error && (
        <p role="alert" className="text-sm text-red-600 dark:text-red-400">
          {error}
        </p>
      )}
    </div>
  );
}
