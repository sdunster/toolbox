import { deliveryStatusLabel, type StatusTone } from "./deliveryStatus";

/** Mirrors `broadcast::MAX_BROADCAST_RECIPIENTS` in the API. */
export const MAX_BROADCAST_RECIPIENTS = 100;

/**
 * Split pasted recipients — one per line, or separated by commas,
 * semicolons or spaces — into a lowercased, de-duplicated list in the order
 * first seen. A preview only: the server normalizes and validates again and
 * is the only authority (`broadcast::normalize_recipients`).
 */
export function parseRecipients(text: string): string[] {
  const seen = new Set<string>();
  const out: string[] = [];
  for (const part of text.split(/[\s,;]+/)) {
    // Tolerate a pasted `Name <addr@example.com>` by keeping the address.
    const email = part.replace(/^<|>$/g, "").trim().toLowerCase();
    if (!email || seen.has(email)) continue;
    seen.add(email);
    out.push(email);
  }
  return out;
}

/** Entries without an `@` with text on both sides — flagged before sending. */
export function invalidRecipients(recipients: string[]): string[] {
  return recipients.filter((r) => !/^[^@\s]+@[^@\s]+$/.test(r));
}

export type BroadcastSendStatus = "PENDING" | "SENT" | "FAILED" | "SUPPRESSED";

/**
 * One label for a recipient: the provider-reported delivery outcome when
 * there is one, else what happened at send time.
 */
export function recipientStatus(
  sendStatus: string,
  deliveryStatus: string | null | undefined,
): { label: string; tone: StatusTone } {
  const delivery = deliveryStatusLabel(deliveryStatus);
  if (delivery) return delivery;
  switch (sendStatus) {
    case "SENT":
      return { label: "Sent", tone: "neutral" };
    case "FAILED":
      return { label: "Failed to send", tone: "bad" };
    case "SUPPRESSED":
      // On the suppression list: a past hard bounce or spam complaint.
      return { label: "Skipped (suppressed)", tone: "bad" };
    default:
      return { label: "Not sent", tone: "warn" };
  }
}
