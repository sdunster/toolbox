/**
 * Persists which instance the switcher last had selected, so a reload (or a
 * fresh `/app` visit) lands back where the user left off instead of always
 * defaulting to their first membership.
 */
const SELECTED_INSTANCE_KEY = "mt_selected_instance_id";

export function getStoredSelectedInstanceId(): string | null {
  return localStorage.getItem(SELECTED_INSTANCE_KEY);
}

export function setStoredSelectedInstanceId(instanceId: string): void {
  localStorage.setItem(SELECTED_INSTANCE_KEY, instanceId);
}

export interface SelectedInstance {
  id: string;
  slug: string;
  name: string;
  /** The signed-in user's role in this instance — drives owner-only UI
   * (the Deleted list, the full assignee picker) without a separate query. */
  role: "OWNER" | "AGENT";
  /** Support (tickets) or finance (projects/invoices) — decides which nav
   * and which home page the shell shows. Always taken from the fresh
   * membership data, never from `localStorage` (only the id is stored), so a
   * selection saved before `kind` existed still resolves correctly. */
  kind: InstanceKind;
}

export type InstanceKind = "SUPPORT" | "FINANCE";

/** Narrows the generated Relay enum (which also admits
 * `"%future added value"`) to the two kinds this build knows about —
 * anything unrecognised is treated as support, today's only other kind. */
export function toInstanceKind(kind: string): InstanceKind {
  return kind === "FINANCE" ? "FINANCE" : "SUPPORT";
}

/** Where a kind's section of the app starts: the switcher navigates here
 * when the selection changes kind, and `/app`'s index redirects here. */
export function homePathForKind(kind: InstanceKind): string {
  return kind === "FINANCE" ? "/app/invoices" : "/app/tickets/open";
}
