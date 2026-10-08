import { graphql, useFragment } from "react-relay";
import { Link } from "react-router";
import type { TicketThread_ticket$key } from "./__generated__/TicketThread_ticket.graphql";
import { MessageItem } from "./MessageItem";
import { StatusControl } from "./StatusControl";
import { AssigneeControl } from "./AssigneeControl";
import { RequesterCcEditor } from "./RequesterCcEditor";
import { ReplyForm } from "./ReplyForm";
import { NoteForm } from "./NoteForm";

const ticketThreadFragment = graphql`
  fragment TicketThread_ticket on Ticket {
    id
    number
    subject
    createdAt
    messages {
      id
      ...MessageItem_message
    }
    ...StatusControl_ticket
    ...AssigneeControl_ticket
    ...RequesterCcEditor_ticket
  }
`;

/**
 * The thread view's body: header, message list (oldest first, per the
 * schema's `Ticket.messages` ordering), requester/CC editors, and the two
 * separate reply/note boxes. `Ticket.messages` is a plain list in the
 * schema (no connection, no pagination args) — unlike the ticket lists,
 * there is no `usePaginationFragment` here because the API gives us
 * nothing to paginate against; see the step 8b report.
 */
export function TicketThread({ ticket }: { ticket: TicketThread_ticket$key }) {
  const data = useFragment(ticketThreadFragment, ticket);

  return (
    <div className="mx-auto flex max-w-3xl flex-col gap-6">
      <Link to="/app/tickets/open" className="text-sm text-ink-muted underline">
        ← Back to tickets
      </Link>

      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <h1 className="text-xl font-semibold text-ink-strong">
            #{data.number} {data.subject}
          </h1>
          <p className="mt-1 text-xs text-ink-muted">
            Opened {new Date(data.createdAt * 1000).toLocaleString()}
          </p>
        </div>
        <div className="flex flex-col items-end gap-2">
          <StatusControl ticket={data} />
          <AssigneeControl ticket={data} />
        </div>
      </div>

      <RequesterCcEditor ticket={data} />

      <ul className="flex flex-col gap-3" aria-label="Message thread">
        {data.messages.map((message) => (
          <MessageItem key={message.id} message={message} />
        ))}
      </ul>

      <ReplyForm ticketId={data.id} />
      <NoteForm ticketId={data.id} />
    </div>
  );
}
