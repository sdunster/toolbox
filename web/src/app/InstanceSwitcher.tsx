import { useEffect, useMemo } from "react";
import { graphql, useFragment } from "react-relay";
import { useLocation, useNavigate } from "react-router";
import type {
  InstanceSwitcher_user$data,
  InstanceSwitcher_user$key,
} from "./__generated__/InstanceSwitcher_user.graphql";
import {
  getStoredSelectedInstanceId,
  homePathForKind,
  setStoredSelectedInstanceId,
  toInstanceKind,
  type SelectedInstance,
} from "./selectedInstance";

const instanceSwitcherFragment = graphql`
  fragment InstanceSwitcher_user on User {
    memberships {
      role
      instance {
        id
        name
        slug
        kind
      }
    }
  }
`;

type Membership = InstanceSwitcher_user$data["memberships"][number];

/** Pages that belong to neither kind — switching kind while on one of these
 * keeps you there; only the nav changes. */
const KIND_NEUTRAL_PATHS = ["/app/settings", "/app/admin"];

function isKindNeutral(pathname: string): boolean {
  return KIND_NEUTRAL_PATHS.some(
    (p) => pathname === p || pathname.startsWith(`${p}/`),
  );
}

function toSelected(m: Membership): SelectedInstance {
  const { id, name, slug, kind } = m.instance;
  return {
    id,
    name,
    slug,
    role: m.role as "OWNER" | "AGENT",
    kind: toInstanceKind(kind),
  };
}

function Option({ membership }: { membership: Membership }) {
  return (
    <option value={membership.instance.id}>{membership.instance.name}</option>
  );
}

/**
 * Which instance the signed-in user is currently working. Reads
 * `me.memberships` (via its own colocated fragment) and persists the choice
 * to `localStorage` so a reload keeps it. Calls `onChange` whenever the
 * effective selection changes — including the very first render, once
 * memberships are known — rather than owning the selected value itself, so
 * `AppShell` can thread it to nested routes via `SelectedInstanceContext`.
 *
 * Only the instance *id* is persisted; its kind and role always come from
 * the membership data just fetched, so a selection stored before `kind`
 * existed keeps working. A user who belongs to both support and finance
 * instances sees them grouped under two `<optgroup>`s; anyone with only one
 * kind gets the plain flat list. Picking an instance of a *different* kind
 * than the current one navigates to that kind's home page — the page being
 * viewed (a ticket queue, a project) has no counterpart in the other kind.
 * The user's own settings and the admin area belong to neither kind, so
 * switching from one of those stays put.
 */
export function InstanceSwitcher({
  user,
  selected,
  onChange,
}: {
  user: InstanceSwitcher_user$key;
  selected: SelectedInstance | null;
  onChange: (instance: SelectedInstance) => void;
}) {
  const data = useFragment(instanceSwitcherFragment, user);
  const memberships = data.memberships;
  const navigate = useNavigate();
  const { pathname } = useLocation();

  const initialInstanceId = useMemo(() => {
    const stored = getStoredSelectedInstanceId();
    if (stored && memberships.some((m) => m.instance.id === stored)) {
      return stored;
    }
    return memberships[0]?.instance.id ?? null;
  }, [memberships]);

  useEffect(() => {
    if (selected || !initialInstanceId) return;
    const match = memberships.find((m) => m.instance.id === initialInstanceId);
    if (match) onChange(toSelected(match));
  }, [initialInstanceId, memberships, selected, onChange]);

  if (memberships.length === 0) {
    return <p className="text-sm text-ink-muted">No instances</p>;
  }

  function handleChange(instanceId: string) {
    const match = memberships.find((m) => m.instance.id === instanceId);
    if (!match) return;
    setStoredSelectedInstanceId(instanceId);
    const next = toSelected(match);
    onChange(next);
    if (selected && selected.kind !== next.kind && !isKindNeutral(pathname)) {
      navigate(homePathForKind(next.kind));
    }
  }

  const selectedRole = memberships.find(
    (m) => m.instance.id === selected?.id,
  )?.role;

  const support = memberships.filter(
    (m) => toInstanceKind(m.instance.kind) === "SUPPORT",
  );
  const finance = memberships.filter(
    (m) => toInstanceKind(m.instance.kind) === "FINANCE",
  );
  const grouped = support.length > 0 && finance.length > 0;

  return (
    <label className="flex items-center gap-2 text-sm">
      <span className="sr-only">Instance</span>
      <select
        aria-label="Instance"
        className="rounded-md border border-line bg-surface px-2 py-1.5 text-sm text-ink"
        value={selected?.id ?? ""}
        onChange={(e) => handleChange(e.target.value)}
      >
        {grouped ? (
          <>
            <optgroup label="Support">
              {support.map((m) => (
                <Option key={m.instance.id} membership={m} />
              ))}
            </optgroup>
            <optgroup label="Finance">
              {finance.map((m) => (
                <Option key={m.instance.id} membership={m} />
              ))}
            </optgroup>
          </>
        ) : (
          memberships.map((m) => <Option key={m.instance.id} membership={m} />)
        )}
      </select>
      {selectedRole && (
        <span className="rounded-full bg-surface-sunken px-2 py-0.5 text-xs text-ink-muted capitalize">
          {selectedRole.toLowerCase()}
        </span>
      )}
    </label>
  );
}
