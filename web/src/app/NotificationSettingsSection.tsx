import { useState } from "react";
import { graphql, useFragment, useMutation } from "react-relay";
import type { RecordSourceSelectorProxy } from "relay-runtime";
import type {
  NotificationSettingsSection_user$data,
  NotificationSettingsSection_user$key,
} from "./__generated__/NotificationSettingsSection_user.graphql";
import type {
  NotificationSettingsSectionUpdateMutation,
  NotificationSettingsInput,
} from "./__generated__/NotificationSettingsSectionUpdateMutation.graphql";
import { relayMutationErrorMessage } from "../lib/relayMutationError";

const notificationSettingsSectionFragment = graphql`
  fragment NotificationSettingsSection_user on User {
    memberships {
      instance {
        id
        name
        kind
      }
      notificationSettings {
        newTicket
        assignedToMe
        assignedToMeUpdated
        unassignedUpdated
        assignedToOthersUpdated
      }
    }
  }
`;

type SettingKey = keyof NotificationSettingsInput;

const TOGGLES: { key: SettingKey; label: string }[] = [
  { key: "newTicket", label: "A new ticket is created" },
  { key: "assignedToMe", label: "A ticket is assigned to me" },
  { key: "assignedToMeUpdated", label: "A ticket assigned to me is updated" },
  { key: "unassignedUpdated", label: "An unassigned ticket is updated" },
  {
    key: "assignedToOthersUpdated",
    label: "A ticket assigned to someone else is updated",
  },
];

type Settings =
  NotificationSettingsSection_user$data["memberships"][number]["notificationSettings"];

/**
 * Read one field by name. A plain `settings[key]` computed index reads the
 * same value but leaves `relay/unused-fields` unable to see that this file
 * actually uses each queried field — this explicit switch is what keeps the
 * fragment's field list and this component's actual usage in sync.
 */
function valueFor(settings: Settings, key: SettingKey): boolean {
  switch (key) {
    case "newTicket":
      return settings.newTicket;
    case "assignedToMe":
      return settings.assignedToMe;
    case "assignedToMeUpdated":
      return settings.assignedToMeUpdated;
    case "unassignedUpdated":
      return settings.unassignedUpdated;
    case "assignedToOthersUpdated":
      return settings.assignedToOthersUpdated;
  }
}

/** Every field but `key` unset — a patch that touches exactly one setting. */
function patchFor(key: SettingKey, value: boolean): NotificationSettingsInput {
  return {
    newTicket: undefined,
    assignedToMe: undefined,
    assignedToMeUpdated: undefined,
    unassignedUpdated: undefined,
    assignedToOthersUpdated: undefined,
    [key]: value,
  };
}

/**
 * Email notification preferences — one block per *support* instance the
 * caller belongs to, five checkboxes each (finance instances have no
 * tickets to notify about, so they're left out).
 * `MembershipInfo.notificationSettings` is self-only server-side (see its
 * doc comment), which this only ever reads via `me`, so that guard is never
 * exercised here.
 *
 * `MembershipInfo` carries no `id` field, so a toggle's mutation response
 * can't be merged onto the query's `me.memberships` list by Relay's usual
 * id-keyed matching (the same reason `PasskeyList`'s delete uses a manual
 * updater instead of an id-keyed merge). Both `optimisticUpdater` and
 * `updater` below locate the right membership record by its linked
 * `instance.id` and set the one field that changed directly.
 */
export function NotificationSettingsSection({
  user,
}: {
  user: NotificationSettingsSection_user$key;
}) {
  const data = useFragment(notificationSettingsSectionFragment, user);
  const [commit] = useMutation<NotificationSettingsSectionUpdateMutation>(
    graphql`
      mutation NotificationSettingsSectionUpdateMutation(
        $instanceId: ID!
        $settings: NotificationSettingsInput!
      ) {
        updateNotificationSettings(
          instanceId: $instanceId
          settings: $settings
        ) {
          notificationSettings {
            newTicket
            assignedToMe
            assignedToMeUpdated
            unassignedUpdated
            assignedToOthersUpdated
          }
        }
      }
    `,
  );
  const [pending, setPending] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);

  function toggle(instanceId: string, key: SettingKey, next: boolean) {
    const pendingId = `${instanceId}:${key}`;
    setError(null);
    setPending(pendingId);
    const applyLocally = (store: RecordSourceSelectorProxy) => {
      const me = store.getRoot().getLinkedRecord("me");
      const memberships = me?.getLinkedRecords("memberships") ?? [];
      const membership = memberships.find(
        (m) => m?.getLinkedRecord("instance")?.getValue("id") === instanceId,
      );
      membership?.getLinkedRecord("notificationSettings")?.setValue(next, key);
    };
    commit({
      variables: { instanceId, settings: patchFor(key, next) },
      optimisticUpdater: applyLocally,
      updater: applyLocally,
      onError: (err) => {
        setPending(null);
        setError(
          relayMutationErrorMessage(
            err,
            "Failed to update notification settings.",
          ),
        );
      },
      onCompleted: () => setPending(null),
    });
  }

  // Staff notifications are about ticket activity, which a finance
  // instance never has — its memberships would only be five toggles that
  // can never fire.
  const supportMemberships = data.memberships.filter(
    (m) => m.instance.kind !== "FINANCE",
  );

  if (data.memberships.length === 0) {
    return (
      <p className="text-sm text-ink-muted">
        You have no instance memberships yet.
      </p>
    );
  }

  if (supportMemberships.length === 0) {
    return (
      <p className="text-sm text-ink-muted">
        Email notifications are about ticket activity, so they only apply to
        support instances — and you aren&apos;t a member of any.
      </p>
    );
  }

  return (
    <div className="flex flex-col gap-6">
      <p className="text-sm text-ink-muted">
        Updates are customer messages, replies, internal notes and status
        changes. You&apos;re never emailed about your own actions.
      </p>

      {error && (
        <p role="alert" className="text-sm text-red-600 dark:text-red-400">
          {error}
        </p>
      )}

      {supportMemberships.map((membership) => (
        <div key={membership.instance.id} className="flex flex-col gap-2">
          <h3 className="text-sm font-medium text-ink">
            {membership.instance.name}
          </h3>
          <ul className="flex flex-col gap-2">
            {TOGGLES.map(({ key, label }) => {
              const pendingId = `${membership.instance.id}:${key}`;
              return (
                <li key={key}>
                  <label className="flex items-center gap-2 text-sm text-ink">
                    <input
                      type="checkbox"
                      checked={valueFor(membership.notificationSettings, key)}
                      disabled={pending === pendingId}
                      onChange={(e) =>
                        toggle(membership.instance.id, key, e.target.checked)
                      }
                      className="size-4 rounded-sm border-line text-accent focus:ring-2 focus:ring-accent/25"
                    />
                    {label}
                  </label>
                </li>
              );
            })}
          </ul>
        </div>
      ))}
    </div>
  );
}
