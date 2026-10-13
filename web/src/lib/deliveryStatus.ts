/**
 * What the mail provider reported for one recipient — the API's
 * `DeliveryStatusType`, shared by broadcast recipients and ticket messages.
 */
export type DeliveryStatus =
  "DELAYED" | "DELIVERED" | "BOUNCED" | "REJECTED" | "COMPLAINED";

export type StatusTone = "neutral" | "good" | "warn" | "bad";

/** A label and tone for a delivery status, or `null` for an unknown one. */
export function deliveryStatusLabel(
  status: string | null | undefined,
): { label: string; tone: StatusTone } | null {
  switch (status) {
    case "DELIVERED":
      return { label: "Delivered", tone: "good" };
    case "DELAYED":
      return { label: "Delayed", tone: "warn" };
    case "BOUNCED":
      return { label: "Bounced", tone: "bad" };
    case "REJECTED":
      return { label: "Rejected", tone: "bad" };
    case "COMPLAINED":
      return { label: "Marked as spam", tone: "bad" };
    default:
      return null;
  }
}

export const statusToneClass: Record<StatusTone, string> = {
  neutral: "bg-surface-sunken text-ink-muted",
  good: "bg-green-100 text-green-800 dark:bg-green-950 dark:text-green-300",
  warn: "bg-amber-100 text-amber-800 dark:bg-amber-950 dark:text-amber-300",
  bad: "bg-red-100 text-red-800 dark:bg-red-950 dark:text-red-300",
};
