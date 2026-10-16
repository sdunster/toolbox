import { useId, useState } from "react";
import { graphql, useMutation } from "react-relay";
import type { BroadcastSendersSectionAddMutation } from "./__generated__/BroadcastSendersSectionAddMutation.graphql";
import type { BroadcastSendersSectionDeleteMutation } from "./__generated__/BroadcastSendersSectionDeleteMutation.graphql";
import { Button } from "../../components/ui/Button";
import { FormField } from "../../components/ui/FormField";
import TextInput from "../../components/ui/TextInput";
import { formatSender, type BroadcastSender } from "../../lib/broadcast";
import { relayMutationErrorMessage } from "../../lib/relayMutationError";

/**
 * Saved senders (`addBroadcastSender`/`deleteBroadcastSender`) — a From name
 * and address to pick in `BroadcastForm` instead of typing them each time.
 * Both mutations return the instance, so Relay merges the new
 * `broadcastSenders` list by id with no updater. Owner-only in the API, like
 * the rest of the page.
 */
export function BroadcastSendersSection({
  instanceId,
  senders,
  domains,
}: {
  instanceId: string;
  senders: readonly BroadcastSender[];
  domains: readonly string[];
}) {
  const id = useId();
  const [name, setName] = useState("");
  const [address, setAddress] = useState("");
  const [error, setError] = useState<string | null>(null);

  const [addSender, adding] = useMutation<BroadcastSendersSectionAddMutation>(
    graphql`
      mutation BroadcastSendersSectionAddMutation(
        $instanceId: ID!
        $name: String!
        $address: String!
      ) {
        addBroadcastSender(
          instanceId: $instanceId
          name: $name
          address: $address
        ) {
          id
          broadcastSenders {
            id
            name
            address
          }
        }
      }
    `,
  );

  const [deleteSender, deleting] =
    useMutation<BroadcastSendersSectionDeleteMutation>(graphql`
      mutation BroadcastSendersSectionDeleteMutation(
        $instanceId: ID!
        $senderId: ID!
      ) {
        deleteBroadcastSender(instanceId: $instanceId, senderId: $senderId) {
          id
          broadcastSenders {
            id
            name
            address
          }
        }
      }
    `);

  function handleAdd(e: React.FormEvent) {
    e.preventDefault();
    if (!name.trim() || !address.trim() || adding) return;
    setError(null);
    addSender({
      variables: { instanceId, name: name.trim(), address: address.trim() },
      onCompleted: () => {
        setName("");
        setAddress("");
      },
      onError: (err) =>
        setError(relayMutationErrorMessage(err, "Couldn't save the sender.")),
    });
  }

  function handleDelete(sender: BroadcastSender) {
    if (!window.confirm(`Remove ${formatSender(sender)}?`)) return;
    setError(null);
    deleteSender({
      variables: { instanceId, senderId: sender.id },
      onError: (err) =>
        setError(relayMutationErrorMessage(err, "Couldn't remove the sender.")),
    });
  }

  return (
    <section
      aria-label="Saved senders"
      className="flex flex-col gap-3 rounded-lg border border-line p-4"
    >
      <div>
        <h2 className="text-base font-semibold text-ink-strong">
          Saved senders
        </h2>
        <p className="mt-1 text-sm text-ink-muted">
          A name and address to pick when you write a broadcast. The address
          must be on one of this instance&apos;s domains.
        </p>
      </div>

      {senders.length === 0 ? (
        <p className="text-sm text-ink-muted">No saved senders yet.</p>
      ) : (
        <ul className="flex flex-col divide-y divide-line-faint">
          {senders.map((s) => (
            <li
              key={s.id}
              className="flex items-center justify-between gap-2 py-2"
            >
              <span className="text-sm break-all text-ink">
                {formatSender(s)}
              </span>
              <Button
                variant="danger"
                disabled={deleting}
                onClick={() => handleDelete(s)}
              >
                Remove
              </Button>
            </li>
          ))}
        </ul>
      )}

      <form
        onSubmit={handleAdd}
        className="grid gap-3 border-t border-line-faint pt-3 sm:grid-cols-[1fr_1fr_auto] sm:items-end"
      >
        <FormField label="Sender name" htmlFor={`${id}-name`}>
          <TextInput
            id={`${id}-name`}
            placeholder="Product updates"
            value={name}
            onChange={(e) => setName(e.target.value)}
            disabled={adding}
          />
        </FormField>
        <FormField label="Sender address" htmlFor={`${id}-address`}>
          <TextInput
            id={`${id}-address`}
            placeholder={`updates@${domains[0] ?? "example.com"}`}
            value={address}
            onChange={(e) => setAddress(e.target.value)}
            disabled={adding}
          />
        </FormField>
        <Button
          type="submit"
          variant="secondary"
          disabled={!name.trim() || !address.trim() || adding}
        >
          {adding ? "Saving…" : "Save sender"}
        </Button>
      </form>

      {error && (
        <p role="alert" className="text-sm text-red-600 dark:text-red-400">
          {error}
        </p>
      )}
    </section>
  );
}
