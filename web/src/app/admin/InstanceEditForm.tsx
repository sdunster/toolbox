import { useState } from "react";
import { graphql, useFragment, useMutation } from "react-relay";
import type { InstanceEditForm_instance$key } from "./__generated__/InstanceEditForm_instance.graphql";
import type { InstanceEditFormMutation } from "./__generated__/InstanceEditFormMutation.graphql";
import { relayMutationErrorMessage } from "../../lib/relayMutationError";
import { Card } from "../../components/ui/Card";
import { Button } from "../../components/ui/Button";
import { FormField } from "../../components/ui/FormField";
import TextInput from "../../components/ui/TextInput";

const instanceEditFormFragment = graphql`
  fragment InstanceEditForm_instance on Instance {
    id
    name
    slug
    kind
    fromName
    signature
    publicSubmissionEnabled
  }
`;

/**
 * Name / from-name / signature / public-submission. No slug input —
 * `updateInstance` doesn't take one either: slugs are immutable once
 * created (a rename would break the `[#{slug}-N]` subject tags already
 * stamped on this instance's past tickets and emailed to requesters) — see
 * `updateInstance`'s doc comment. Shown read-only, with that reason, rather
 * than left off the page entirely.
 *
 * The kind is shown read-only too (it's fixed at creation). From name,
 * signature and public submission are support-only — outbound mail and the
 * `/submit` form don't exist for a finance instance — so they're hidden
 * for one, and resubmitted unchanged since `updateInstance` still takes
 * them.
 */
export function InstanceEditForm({
  instance,
}: {
  instance: InstanceEditForm_instance$key;
}) {
  const data = useFragment(instanceEditFormFragment, instance);
  const isSupport = data.kind !== "FINANCE";
  const [name, setName] = useState(data.name);
  const [fromName, setFromName] = useState(data.fromName);
  const [signature, setSignature] = useState(data.signature);
  const [publicSubmissionEnabled, setPublicSubmissionEnabled] = useState(
    data.publicSubmissionEnabled,
  );
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  const [commit, isSaving] = useMutation<InstanceEditFormMutation>(graphql`
    mutation InstanceEditFormMutation(
      $id: ID!
      $name: String!
      $fromName: String!
      $signature: String!
      $publicSubmissionEnabled: Boolean!
    ) {
      updateInstance(
        id: $id
        name: $name
        fromName: $fromName
        signature: $signature
        publicSubmissionEnabled: $publicSubmissionEnabled
      ) {
        id
        name
        fromName
        signature
        publicSubmissionEnabled
      }
    }
  `);

  function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (isSaving) return;
    setError(null);
    setSaved(false);
    commit({
      variables: {
        id: data.id,
        name: name.trim(),
        fromName,
        signature,
        publicSubmissionEnabled,
      },
      onCompleted: () => setSaved(true),
      onError: (err) =>
        setError(relayMutationErrorMessage(err, "Failed to save instance.")),
    });
  }

  return (
    <Card>
      <h2 className="mb-4 text-sm font-semibold tracking-wide text-ink-muted uppercase">
        Details
      </h2>
      <form onSubmit={handleSubmit} className="flex flex-col gap-4">
        <FormField label="Name" htmlFor="instance-name">
          <TextInput
            id="instance-name"
            value={name}
            onChange={(e) => setName(e.target.value)}
            required
          />
        </FormField>
        <FormField label="Slug" htmlFor="instance-slug">
          <TextInput
            id="instance-slug"
            value={data.slug}
            readOnly
            className="bg-surface-sunken text-ink-muted"
          />
          <p className="mt-1 text-xs text-ink-muted">
            {isSupport ? (
              <>
                Can&apos;t be changed here — renaming it would break the
                &quot;[#{data.slug}-N]&quot; subject tags already stamped on
                this instance&apos;s past tickets.
              </>
            ) : (
              <>Can&apos;t be changed once created.</>
            )}
          </p>
        </FormField>
        <FormField label="Kind" htmlFor="instance-kind">
          <TextInput
            id="instance-kind"
            value={isSupport ? "Support" : "Finance"}
            readOnly
            className="bg-surface-sunken text-ink-muted"
          />
        </FormField>
        {isSupport && (
          <>
            <FormField label="From name" htmlFor="instance-from-name">
              <TextInput
                id="instance-from-name"
                value={fromName}
                onChange={(e) => setFromName(e.target.value)}
                required
              />
            </FormField>
            <FormField label="Signature" htmlFor="instance-signature">
              <textarea
                id="instance-signature"
                className="w-full rounded-md border border-line bg-surface px-3 py-2 text-sm text-ink transition-colors focus:border-accent focus:ring-2 focus:ring-accent/25 focus:outline-none"
                rows={3}
                value={signature}
                onChange={(e) => setSignature(e.target.value)}
              />
            </FormField>
            <label className="flex items-center gap-2 text-sm text-ink">
              <input
                type="checkbox"
                checked={publicSubmissionEnabled}
                onChange={(e) => setPublicSubmissionEnabled(e.target.checked)}
                className="size-4 rounded-sm border-line text-accent focus:ring-2 focus:ring-accent/25"
              />
              Public submission enabled
            </label>
          </>
        )}

        {error && (
          <p role="alert" className="text-sm text-red-600 dark:text-red-400">
            {error}
          </p>
        )}
        {saved && !error && (
          <p className="text-sm text-green-700 dark:text-green-400">Saved.</p>
        )}

        <Button type="submit" disabled={isSaving} className="self-start">
          {isSaving ? "Saving…" : "Save"}
        </Button>
      </form>
    </Card>
  );
}
