import { Suspense } from "react";
import { graphql } from "react-relay";
import { useParams } from "react-router";
import type { TicketThreadPageQuery } from "./__generated__/TicketThreadPageQuery.graphql";
import { useRetryableLazyLoadQuery } from "../../components/useRetryableLazyLoadQuery";
import RelayErrorBoundary from "../../components/RelayErrorBoundary";
import LoadingIndicator from "../../components/LoadingIndicator";
import { ButtonLink } from "../../components/ui/Button";
import { TicketThread } from "./TicketThread";

const ticketThreadPageQuery = graphql`
  query TicketThreadPageQuery($id: ID!) @throwOnFieldError {
    ticket(id: $id) {
      id
      ...TicketThread_ticket
    }
  }
`;

function Content({ id }: { id: string }) {
  const data = useRetryableLazyLoadQuery<TicketThreadPageQuery>(
    ticketThreadPageQuery,
    { id },
  );

  if (!data.ticket) {
    return (
      <div className="rounded-lg border border-dashed border-line p-10 text-center text-ink-muted">
        <p className="font-medium text-ink">Ticket not found</p>
        <p className="mt-1 text-sm">
          It may not exist, or you may not have access to it.
        </p>
        <ButtonLink to="/app/tickets/open" variant="secondary" className="mt-4">
          Back to tickets
        </ButtonLink>
      </div>
    );
  }

  return <TicketThread ticket={data.ticket} />;
}

/** `/app/tickets/:id` — the thread view. */
export function TicketThreadPage() {
  const { id } = useParams<{ id: string }>();

  if (!id) return null;

  return (
    <RelayErrorBoundary canRetry>
      <Suspense fallback={<LoadingIndicator />}>
        <Content id={id} />
      </Suspense>
    </RelayErrorBoundary>
  );
}
