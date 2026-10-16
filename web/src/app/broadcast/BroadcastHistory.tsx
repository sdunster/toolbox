import { graphql, useFragment } from "react-relay";
import type { BroadcastHistory_broadcasts$key } from "./__generated__/BroadcastHistory_broadcasts.graphql";
import { formatTimestamp } from "../../lib/dates";
import { formatClockTime, recipientStatus } from "../../lib/broadcast";
import { ENGAGEMENT_CAVEAT, statusToneClass } from "../../lib/deliveryStatus";

const broadcastHistoryFragment = graphql`
  fragment BroadcastHistory_broadcasts on Broadcast @relay(plural: true) {
    id
    subject
    fromAddress
    createdAt
    recipientCount
    inProgress
    pendingCount
    nextBatchAt
    estimatedFinishAt
    failedCount
    suppressedCount
    deliveredCount
    bouncedCount
    complainedCount
    openedCount
    clickedCount
    createdBy {
      name
      email
    }
    recipients {
      email
      sendStatus
      error
      deliveryStatus
      deliveryDetail
      openedAt
      clickedAt
    }
  }
`;

function counts(b: {
  recipientCount: number;
  inProgress: boolean;
  pendingCount: number;
  deliveredCount: number;
  bouncedCount: number;
  complainedCount: number;
  openedCount: number;
  clickedCount: number;
  failedCount: number;
  suppressedCount: number;
}): string {
  const parts = [
    b.inProgress
      ? `${b.recipientCount - b.pendingCount} of ${b.recipientCount} sent`
      : `${b.recipientCount} sent to`,
  ];
  if (b.deliveredCount) parts.push(`${b.deliveredCount} delivered`);
  if (b.openedCount) parts.push(`${b.openedCount} opened`);
  if (b.clickedCount) parts.push(`${b.clickedCount} clicked a link`);
  if (b.bouncedCount) parts.push(`${b.bouncedCount} bounced`);
  if (b.complainedCount) parts.push(`${b.complainedCount} marked as spam`);
  if (b.failedCount) parts.push(`${b.failedCount} failed`);
  if (b.suppressedCount)
    parts.push(`${b.suppressedCount} skipped (suppressed)`);
  return parts.join(" · ");
}

/** "Sending · next batch at 2:35 pm · done around 4:20 pm". */
function progress(b: {
  nextBatchAt: number | null | undefined;
  estimatedFinishAt: number | null | undefined;
}): string {
  const parts = ["Sending"];
  if (b.nextBatchAt != null)
    parts.push(`next batch at ${formatClockTime(b.nextBatchAt)}`);
  if (b.estimatedFinishAt != null)
    parts.push(`done around ${formatClockTime(b.estimatedFinishAt)}`);
  return parts.join(" · ");
}

/**
 * Past broadcasts, newest first, each expandable to every recipient's
 * status — what happened at send time, then what SES reported (delivered,
 * bounced with the receiving server's reason, marked as spam), and whether
 * that recipient's copy was opened or a link in it clicked (approximate —
 * see `ENGAGEMENT_CAVEAT`). A broadcast still sending shows its progress;
 * the page refetches while one is.
 */
export function BroadcastHistory({
  broadcasts,
}: {
  broadcasts: BroadcastHistory_broadcasts$key;
}) {
  const data = useFragment(broadcastHistoryFragment, broadcasts);

  return (
    <section className="flex flex-col gap-3">
      <h2 className="text-base font-semibold text-ink-strong">
        Sent broadcasts
      </h2>
      {data.length === 0 ? (
        <p className="text-sm text-ink-muted">Nothing sent yet.</p>
      ) : (
        <ul className="flex flex-col gap-2">
          {data.map((b) => (
            <li key={b.id} className="rounded-lg border border-line">
              <details>
                <summary className="flex cursor-pointer flex-col gap-0.5 px-4 py-3">
                  <span className="font-medium text-ink">{b.subject}</span>
                  <span className="text-xs text-ink-muted">
                    {formatTimestamp(b.createdAt)} · from {b.fromAddress}
                    {b.createdBy
                      ? ` by ${b.createdBy.name || b.createdBy.email}`
                      : ""}
                  </span>
                  <span className="text-xs text-ink-muted">{counts(b)}</span>
                  {b.inProgress && (
                    <span className="text-xs font-medium text-ink">
                      {progress(b)}
                    </span>
                  )}
                </summary>
                <table className="w-full border-t border-line text-sm">
                  <tbody>
                    {b.recipients.map((r) => {
                      const status = recipientStatus(
                        r.sendStatus,
                        r.deliveryStatus,
                        b.inProgress,
                      );
                      return (
                        <tr
                          key={r.email}
                          className="border-b border-line last:border-0"
                        >
                          <td className="px-4 py-2 break-all text-ink">
                            {r.email}
                          </td>
                          <td className="px-4 py-2">
                            <span
                              className={`rounded-sm px-2 py-0.5 text-xs font-medium ${statusToneClass[status.tone]}`}
                            >
                              {status.label}
                            </span>
                            {r.openedAt != null && (
                              <span
                                title={`First opened ${formatTimestamp(r.openedAt)}. ${ENGAGEMENT_CAVEAT}`}
                                className={`ml-1 rounded-sm px-2 py-0.5 text-xs font-medium ${statusToneClass.neutral}`}
                              >
                                Opened
                              </span>
                            )}
                            {r.clickedAt != null && (
                              <span
                                title={`First clicked ${formatTimestamp(r.clickedAt)}.`}
                                className={`ml-1 rounded-sm px-2 py-0.5 text-xs font-medium ${statusToneClass.neutral}`}
                              >
                                Clicked
                              </span>
                            )}
                          </td>
                          <td className="px-4 py-2 text-xs break-all text-ink-muted">
                            {r.deliveryDetail ?? r.error ?? ""}
                          </td>
                        </tr>
                      );
                    })}
                  </tbody>
                </table>
              </details>
            </li>
          ))}
        </ul>
      )}
    </section>
  );
}
