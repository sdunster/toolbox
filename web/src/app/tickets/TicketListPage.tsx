import { Suspense, useState } from "react";
import { graphql } from "react-relay";
import type {
  TicketListPageQuery,
  TicketStatusFilterType,
} from "./__generated__/TicketListPageQuery.graphql";
import { useSelectedInstance } from "../SelectedInstanceContext";
import { useCurrentUser } from "../../auth/useCurrentUser";
import { useRetryableLazyLoadQuery } from "../../components/useRetryableLazyLoadQuery";
import RelayErrorBoundary from "../../components/RelayErrorBoundary";
import LoadingIndicator from "../../components/LoadingIndicator";
import { ButtonLink } from "../../components/ui/Button";
import { TicketList } from "./TicketList";

const ticketListPageQuery = graphql`
  query TicketListPageQuery(
    $instanceId: ID!
    $status: TicketStatusFilterType!
    $assignedTo: ID
  ) @throwOnFieldError {
    ...TicketList_query
      @arguments(
        instanceId: $instanceId
        status: $status
        assignedTo: $assignedTo
      )
  }
`;

interface TicketListPageProps {
  status: TicketStatusFilterType;
  /** `true` for the "Mine" tab — resolved to the current user's id below. */
  assignedToMe?: boolean;
  title: string;
  emptyMessage: string;
}

function Content({
  instanceId,
  status,
  assignedTo,
  title,
  emptyMessage,
}: {
  instanceId: string;
  status: TicketStatusFilterType;
  assignedTo: string | null;
  title: string;
  emptyMessage: string;
}) {
  // Captured once per mount (this component remounts only when `instanceId`
  // changes, via the `key` below) and deliberately NOT kept in sync with
  // later `status`/`assignedTo` prop changes — those are applied by
  // `TicketList`'s own `refetch`, not by re-running this query. See
  // `TicketList`'s doc comment.
  const [initialStatus] = useState(status);
  const [initialAssignedTo] = useState(assignedTo);

  const data = useRetryableLazyLoadQuery<TicketListPageQuery>(
    ticketListPageQuery,
    { instanceId, status: initialStatus, assignedTo: initialAssignedTo },
  );

  return (
    <div>
      <h1 className="mb-4 text-xl font-semibold text-ink-strong">{title}</h1>
      <TicketList
        query={data}
        status={status}
        assignedTo={assignedTo}
        emptyMessage={emptyMessage}
      />
    </div>
  );
}

/**
 * One of `/app/tickets/{open,closed,all,mine,deleted}`. All five routes
 * render this same component with different `status`/`assignedToMe` props —
 * see `TicketList`'s doc comment for why that matters (no remount on tab
 * switch).
 */
export function TicketListPage({
  status,
  assignedToMe,
  title,
  emptyMessage,
}: TicketListPageProps) {
  const instance = useSelectedInstance();

  if (!instance) {
    return (
      <div className="rounded-lg border border-dashed border-line p-10 text-center text-ink-muted">
        Pick an instance to see its tickets.
      </div>
    );
  }

  // A deep link (or a bookmark) to a ticket queue while a finance
  // instance is selected — `tickets` would just come back empty, which
  // reads like "the queue is clear" rather than "wrong kind of instance".
  if (instance.kind === "FINANCE") {
    return (
      <div className="rounded-lg border border-dashed border-line p-10 text-center text-ink-muted">
        <p className="font-medium text-ink">
          {instance.name} is a finance instance
        </p>
        <p className="mt-1 text-sm">
          It has no tickets — pick a support instance to see a ticket queue.
        </p>
        <ButtonLink to="/app/projects" variant="secondary" className="mt-4">
          Go to projects
        </ButtonLink>
      </div>
    );
  }

  return (
    <RelayErrorBoundary canRetry>
      <Suspense fallback={<LoadingIndicator />}>
        <ContentForInstance
          key={instance.id}
          instanceId={instance.id}
          status={status}
          assignedToMe={assignedToMe ?? false}
          title={title}
          emptyMessage={emptyMessage}
        />
      </Suspense>
    </RelayErrorBoundary>
  );
}

/**
 * Resolves "assigned to me" to the current user's actual id. Split from
 * `TicketListPage` so `useSelectedInstance`'s `null` case above doesn't
 * force a `useCurrentUser()` call before an instance is even selected.
 */
function ContentForInstance(props: {
  instanceId: string;
  status: TicketStatusFilterType;
  assignedToMe: boolean;
  title: string;
  emptyMessage: string;
}) {
  const { assignedToMe, ...rest } = props;
  const currentUser = useCurrentUser();
  return (
    <Content {...rest} assignedTo={assignedToMe ? currentUser.id : null} />
  );
}
