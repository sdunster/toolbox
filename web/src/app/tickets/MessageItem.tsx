import { graphql, useFragment } from "react-relay";
import type { MessageItem_message$key } from "./__generated__/MessageItem_message.graphql";
import { messageKindContainer, messageKindLabel } from "./ticketStyles";
import {
  deliveryStatusLabel,
  ENGAGEMENT_CAVEAT,
  statusToneClass,
} from "../../lib/deliveryStatus";
import { formatTimestamp } from "../../lib/dates";

const messageItemFragment = graphql`
  fragment MessageItem_message on TicketMessage {
    kind
    fromEmail
    author {
      name
      email
    }
    toEmails
    ccEmails
    bodyText
    createdAt
    deliveries {
      email
      status
      detail
    }
    openedAt
    clickedAt
    attachments {
      filename
      contentType
      size
      downloadUrl
    }
  }
`;

type Delivery = {
  readonly email: string;
  readonly status: string;
  readonly detail?: string | null;
};

/**
 * What the mail provider reported for each recipient of a message we sent
 * (`TicketMessage.deliveries`), then whether it was opened or a link in it
 * clicked (`openedAt`/`clickedAt`) — nothing at all until the first event
 * arrives. The reason (a bounce's SMTP diagnostic, say) is in the tooltip.
 * Opens and clicks are for the message as a whole: every recipient shares one
 * copy, so SES can't say which of them opened it.
 */
function DeliveryList({
  deliveries,
  openedAt,
  clickedAt,
  className,
}: {
  deliveries: readonly Delivery[];
  openedAt?: number | null;
  clickedAt?: number | null;
  className?: string;
}) {
  const engagement: { label: string; at: number }[] = [];
  if (openedAt != null) engagement.push({ label: "Opened", at: openedAt });
  if (clickedAt != null)
    engagement.push({ label: "Link clicked", at: clickedAt });
  if (deliveries.length === 0 && engagement.length === 0) return null;
  return (
    <ul
      aria-label="Delivery status"
      className={`flex flex-wrap gap-1.5 text-xs ${className ?? ""}`}
    >
      {deliveries.map((d) => {
        const status = deliveryStatusLabel(d.status);
        if (!status) return null;
        return (
          <li
            key={d.email}
            title={d.detail ?? undefined}
            className={`rounded-sm px-2 py-0.5 font-medium ${statusToneClass[status.tone]}`}
          >
            {d.email}: {status.label}
          </li>
        );
      })}
      {engagement.map((e) => (
        <li
          key={e.label}
          title={`First seen ${formatTimestamp(e.at)}. ${ENGAGEMENT_CAVEAT}`}
          className={`rounded-sm px-2 py-0.5 font-medium ${statusToneClass.neutral}`}
        >
          {e.label}
        </li>
      ))}
    </ul>
  );
}

function formatBytes(size: number): string {
  if (size < 1024) return `${size} B`;
  if (size < 1024 * 1024) return `${(size / 1024).toFixed(1)} KB`;
  return `${(size / (1024 * 1024)).toFixed(1)} MB`;
}

function senderLabel(data: {
  kind: string;
  fromEmail?: string | null;
  author?: { name: string; email: string } | null;
}): string {
  if (data.author) return `${data.author.name} <${data.author.email}>`;
  if (data.fromEmail) return data.fromEmail;
  return data.kind === "SYSTEM" ? "Toolbox" : "Unknown sender";
}

/**
 * One thread entry. `system` notices render as a centered log line — never a
 * bubble attributed to "someone" — and `note` gets the most visually loud
 * treatment of the four (amber, explicitly labelled "Internal note"), per
 * the build plan's hard requirement that an agent must never mistake one
 * for something the requester saw.
 */
export function MessageItem({ message }: { message: MessageItem_message$key }) {
  const data = useFragment(messageItemFragment, message);
  const when = new Date(data.createdAt * 1000).toLocaleString();

  if (data.kind === "SYSTEM") {
    return (
      <li className="flex flex-col items-center py-2">
        <p className="rounded-full bg-surface-sunken px-3 py-1 text-xs text-ink-muted italic">
          {data.bodyText ?? "System notice"}
          <span className="ml-2 not-italic">· {when}</span>
        </p>
        <DeliveryList
          deliveries={data.deliveries}
          openedAt={data.openedAt}
          clickedAt={data.clickedAt}
          className="mt-1 justify-center"
        />
      </li>
    );
  }

  const containerClass =
    messageKindContainer[data.kind] ?? messageKindContainer.INBOUND;
  const labelClass = messageKindLabel[data.kind] ?? messageKindLabel.INBOUND;

  return (
    <li className={`rounded-lg p-4 ${containerClass}`}>
      <div className="flex flex-wrap items-baseline justify-between gap-x-3 gap-y-1">
        <span className={`text-sm font-semibold ${labelClass}`}>
          {data.kind === "NOTE"
            ? "Internal note — not sent to the customer"
            : senderLabel(data)}
        </span>
        <span className="text-xs text-ink-muted">{when}</span>
      </div>

      {data.kind === "NOTE" && (
        <p className="mt-0.5 text-xs text-ink-muted">by {senderLabel(data)}</p>
      )}

      {(data.toEmails.length > 0 || data.ccEmails.length > 0) &&
        data.kind !== "NOTE" && (
          <p className="mt-1 text-xs text-ink-muted">
            {data.toEmails.length > 0 && <>To: {data.toEmails.join(", ")} </>}
            {data.ccEmails.length > 0 && <>Cc: {data.ccEmails.join(", ")}</>}
          </p>
        )}

      <DeliveryList
        deliveries={data.deliveries}
        openedAt={data.openedAt}
        clickedAt={data.clickedAt}
        className="mt-1"
      />

      <div className="mt-2 text-sm whitespace-pre-wrap text-ink">
        {data.bodyText ?? (
          <span className="text-ink-muted italic">(no text body)</span>
        )}
      </div>

      {data.attachments.length > 0 && (
        <ul className="mt-3 flex flex-col gap-1 border-t border-line-faint pt-2">
          {data.attachments.map((att) => (
            <li key={att.filename} className="text-sm">
              <a
                href={att.downloadUrl}
                className="text-accent-dark underline dark:text-accent-light"
                target="_blank"
                rel="noreferrer"
              >
                {att.filename}
              </a>
              <span className="ml-2 text-xs text-ink-muted">
                {att.contentType}, {formatBytes(att.size)}
              </span>
            </li>
          ))}
        </ul>
      )}
    </li>
  );
}
