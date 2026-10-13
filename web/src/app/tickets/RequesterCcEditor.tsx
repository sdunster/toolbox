import { useState } from "react";
import { graphql, useFragment, useMutation } from "react-relay";
import type { RequesterCcEditor_ticket$key } from "./__generated__/RequesterCcEditor_ticket.graphql";
import type { RequesterCcEditorAddRequesterMutation } from "./__generated__/RequesterCcEditorAddRequesterMutation.graphql";
import type { RequesterCcEditorRemoveRequesterMutation } from "./__generated__/RequesterCcEditorRemoveRequesterMutation.graphql";
import type { RequesterCcEditorAddCcMutation } from "./__generated__/RequesterCcEditorAddCcMutation.graphql";
import type { RequesterCcEditorRemoveCcMutation } from "./__generated__/RequesterCcEditorRemoveCcMutation.graphql";
import { Button } from "../../components/ui/Button";
import TextInput from "../../components/ui/TextInput";

const requesterCcEditorFragment = graphql`
  fragment RequesterCcEditor_ticket on Ticket {
    id
    requesterEmails
    ccEmails
    undeliverableEmails {
      email
      reason
      suppressedAt
    }
  }
`;

type Undeliverable = {
  readonly email: string;
  readonly reason: string;
  readonly suppressedAt: number;
};

/** Hover text for an address Toolbox won't mail (`Ticket.undeliverableEmails`). */
function undeliverableTitle(u: Undeliverable): string {
  const why =
    u.reason === "COMPLAINT"
      ? "marked our mail as spam"
      : "bounced permanently (the mailbox doesn't exist or refuses mail)";
  const since = new Date(u.suppressedAt * 1000).toLocaleDateString();
  return `Undeliverable: this address ${why} on ${since}. Replies and notices skip it.`;
}

/** One editable email chip list: requesters (`To`) or CCs. */
function EmailList({
  label,
  emails,
  undeliverable,
  disabled,
  canRemove,
  onAdd,
  onRemove,
}: {
  label: string;
  emails: readonly string[];
  undeliverable: ReadonlyMap<string, Undeliverable>;
  disabled: boolean;
  canRemove: (email: string) => boolean;
  onAdd: (email: string) => void;
  onRemove: (email: string) => void;
}) {
  const [value, setValue] = useState("");

  return (
    <div>
      <h3 className="text-sm font-semibold text-ink-muted">{label}</h3>
      <ul className="mt-1.5 flex flex-wrap gap-1.5">
        {emails.map((email) => {
          const bad = undeliverable.get(email.toLowerCase());
          return (
            <li
              key={email}
              title={bad ? undeliverableTitle(bad) : undefined}
              className={
                bad
                  ? "flex items-center gap-1 rounded-full bg-amber-100 px-2.5 py-1 text-xs text-amber-900 dark:bg-amber-900/40 dark:text-amber-200"
                  : "flex items-center gap-1 rounded-full bg-surface-sunken px-2.5 py-1 text-xs text-ink"
              }
            >
              {bad && <span aria-hidden="true">⚠</span>}
              {email}
              {bad && <span className="sr-only"> (undeliverable)</span>}
              {canRemove(email) && (
                <button
                  type="button"
                  aria-label={`Remove ${email} from ${label}`}
                  className="cursor-pointer text-ink-muted hover:text-ink"
                  disabled={disabled}
                  onClick={() => onRemove(email)}
                >
                  ×
                </button>
              )}
            </li>
          );
        })}
      </ul>
      <form
        className="mt-2 flex gap-2"
        onSubmit={(e) => {
          e.preventDefault();
          const email = value.trim();
          if (!email) return;
          onAdd(email);
          setValue("");
        }}
      >
        <TextInput
          aria-label={`Add ${label.toLowerCase()} email`}
          type="email"
          placeholder="name@example.com"
          value={value}
          onChange={(e) => setValue(e.target.value)}
          disabled={disabled}
        />
        <Button
          type="submit"
          variant="secondary"
          disabled={disabled || !value.trim()}
        >
          Add
        </Button>
      </form>
    </div>
  );
}

/** Requester ("To") and CC editors. A ticket must keep at least one
 * requester — enforced by the API (`CONFLICT`); mirrored here by disabling
 * remove on the last one so the error path is rare, not the primary UX. */
export function RequesterCcEditor({
  ticket,
}: {
  ticket: RequesterCcEditor_ticket$key;
}) {
  const data = useFragment(requesterCcEditorFragment, ticket);
  const [error, setError] = useState<string | null>(null);

  const [addRequester, addingRequester] =
    useMutation<RequesterCcEditorAddRequesterMutation>(graphql`
      mutation RequesterCcEditorAddRequesterMutation(
        $ticketId: ID!
        $email: String!
      ) {
        addTicketRequester(ticketId: $ticketId, email: $email) {
          id
          requesterEmails
          undeliverableEmails {
            email
            reason
            suppressedAt
          }
        }
      }
    `);
  const [removeRequester, removingRequester] =
    useMutation<RequesterCcEditorRemoveRequesterMutation>(graphql`
      mutation RequesterCcEditorRemoveRequesterMutation(
        $ticketId: ID!
        $email: String!
      ) {
        removeTicketRequester(ticketId: $ticketId, email: $email) {
          id
          requesterEmails
          undeliverableEmails {
            email
            reason
            suppressedAt
          }
        }
      }
    `);
  const [addCc, addingCc] = useMutation<RequesterCcEditorAddCcMutation>(graphql`
    mutation RequesterCcEditorAddCcMutation($ticketId: ID!, $email: String!) {
      addTicketCc(ticketId: $ticketId, email: $email) {
        id
        ccEmails
        undeliverableEmails {
          email
          reason
          suppressedAt
        }
      }
    }
  `);
  const [removeCc, removingCc] = useMutation<RequesterCcEditorRemoveCcMutation>(
    graphql`
      mutation RequesterCcEditorRemoveCcMutation(
        $ticketId: ID!
        $email: String!
      ) {
        removeTicketCc(ticketId: $ticketId, email: $email) {
          id
          ccEmails
          undeliverableEmails {
            email
            reason
            suppressedAt
          }
        }
      }
    `,
  );

  const busy = addingRequester || removingRequester || addingCc || removingCc;
  const undeliverable = new Map(
    data.undeliverableEmails.map((u) => [u.email.toLowerCase(), u]),
  );

  return (
    <div className="flex flex-col gap-4">
      <EmailList
        label="Requesters"
        emails={data.requesterEmails}
        undeliverable={undeliverable}
        disabled={busy}
        canRemove={() => data.requesterEmails.length > 1}
        onAdd={(email) => {
          setError(null);
          addRequester({
            variables: { ticketId: data.id, email },
            optimisticResponse: {
              addTicketRequester: {
                id: data.id,
                undeliverableEmails: data.undeliverableEmails,
                requesterEmails: data.requesterEmails.includes(email)
                  ? data.requesterEmails
                  : [...data.requesterEmails, email],
              },
            },
            onError: () => setError("Failed to add requester."),
          });
        }}
        onRemove={(email) => {
          setError(null);
          removeRequester({
            variables: { ticketId: data.id, email },
            optimisticResponse: {
              removeTicketRequester: {
                id: data.id,
                undeliverableEmails: data.undeliverableEmails,
                requesterEmails: data.requesterEmails.filter(
                  (e) => e !== email,
                ),
              },
            },
            onError: () => setError("Failed to remove requester."),
          });
        }}
      />

      <EmailList
        label="CC"
        emails={data.ccEmails}
        undeliverable={undeliverable}
        disabled={busy}
        canRemove={() => true}
        onAdd={(email) => {
          setError(null);
          addCc({
            variables: { ticketId: data.id, email },
            optimisticResponse: {
              addTicketCc: {
                id: data.id,
                undeliverableEmails: data.undeliverableEmails,
                ccEmails: data.ccEmails.includes(email)
                  ? data.ccEmails
                  : [...data.ccEmails, email],
              },
            },
            onError: () => setError("Failed to add CC."),
          });
        }}
        onRemove={(email) => {
          setError(null);
          removeCc({
            variables: { ticketId: data.id, email },
            optimisticResponse: {
              removeTicketCc: {
                id: data.id,
                undeliverableEmails: data.undeliverableEmails,
                ccEmails: data.ccEmails.filter((e) => e !== email),
              },
            },
            onError: () => setError("Failed to remove CC."),
          });
        }}
      />

      {error && (
        <p role="alert" className="text-sm text-red-600 dark:text-red-400">
          {error}
        </p>
      )}
    </div>
  );
}
