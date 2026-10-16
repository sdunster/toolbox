import { deliveryStatusLabel, type StatusTone } from "./deliveryStatus";

/** Mirrors `broadcast::MAX_BROADCAST_RECIPIENTS` in the API. */
export const MAX_BROADCAST_RECIPIENTS = 500;

/**
 * Mirror `broadcast::BATCH_SIZE`/`BATCH_INTERVAL_SECS`: the worker sends a
 * broadcast this many recipients at a time, this far apart. Used only to
 * preview how long a send will take before it is queued — once queued, the
 * API's own `estimatedFinishAt` is what the page shows.
 */
export const BROADCAST_BATCH_SIZE = 5;
export const BROADCAST_BATCH_INTERVAL_SECS = 300;

/** Seconds from the first batch to the last, for `recipients` addresses. */
export function estimatedSendSecs(recipients: number): number {
  const batches = Math.ceil(recipients / BROADCAST_BATCH_SIZE);
  return Math.max(0, batches - 1) * BROADCAST_BATCH_INTERVAL_SECS;
}

/** A rough duration: "under a minute", "25 min", "1 h 55 min". */
export function formatDuration(secs: number): string {
  const minutes = Math.round(secs / 60);
  if (minutes < 1) return "under a minute";
  const h = Math.floor(minutes / 60);
  const m = minutes % 60;
  if (h === 0) return `${m} min`;
  return m === 0 ? `${h} h` : `${h} h ${m} min`;
}

/** Wall-clock time only, for "next batch at …" within the same day. */
export function formatClockTime(seconds: number): string {
  return new Date(seconds * 1000).toLocaleTimeString([], {
    hour: "numeric",
    minute: "2-digit",
  });
}

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

export type BroadcastSendStatus =
  "PENDING" | "SENDING" | "SENT" | "FAILED" | "SUPPRESSED";

/**
 * One label for a recipient: the provider-reported delivery outcome when
 * there is one, else what happened at send time. `inProgress` is the
 * broadcast's: while it is still sending, a `PENDING` recipient is simply
 * queued for a later batch, and a `SENDING` one is mid-send. Once it has
 * finished, a recipient left `SENDING` was cut short mid-send — the API
 * never retries it, since the mail may already have gone out.
 */
export function recipientStatus(
  sendStatus: string,
  deliveryStatus: string | null | undefined,
  inProgress = false,
): { label: string; tone: StatusTone } {
  const delivery = deliveryStatusLabel(deliveryStatus);
  if (delivery) return delivery;
  switch (sendStatus) {
    case "PENDING":
      return inProgress
        ? { label: "Queued", tone: "neutral" }
        : { label: "Not sent", tone: "warn" };
    case "SENDING":
      return inProgress
        ? { label: "Sending…", tone: "neutral" }
        : { label: "Unknown (send interrupted)", tone: "warn" };
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

/** A saved broadcast sender (`Instance.broadcastSenders`). */
export type BroadcastSender = {
  readonly id: string;
  readonly name: string;
  readonly address: string;
};

/** `Name <address>`, as a mail client shows a From line. */
export function formatSender(sender: { name: string; address: string }) {
  return `${sender.name} <${sender.address}>`;
}
