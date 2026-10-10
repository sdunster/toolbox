import { useState } from "react";
import { graphql, useFragment, useMutation } from "react-relay";
import type { ProjectEditForm_project$key } from "./__generated__/ProjectEditForm_project.graphql";
import type { ProjectEditFormMutation } from "./__generated__/ProjectEditFormMutation.graphql";
import { relayMutationErrorMessage } from "../../lib/relayMutationError";
import { Card } from "../../components/ui/Card";
import { Button } from "../../components/ui/Button";
import { FormField } from "../../components/ui/FormField";
import TextInput from "../../components/ui/TextInput";
import { inputBase } from "../../components/ui/inputStyles";
import { ProjectBillingFields } from "./ProjectBillingFields";
import { billingInput, initialBillingValues } from "../../lib/projectBilling";

const projectEditFormFragment = graphql`
  fragment ProjectEditForm_project on Project {
    id
    name
    clientName
    clientAbn
    clientAddress
    reference
    clientEmail
    paymentTermsDays
    defaultUnitPriceCents
    archived
  }
`;

/**
 * Every field of a project, including `archived`. `updateProject` is a full
 * replace, so every submit sends every field; a blank optional field clears
 * it (the API trims it and removes the attribute). The mutation returns the
 * `Project` by `id`, so Relay merges the result onto the page's heading and
 * the list page's cached rows with no updater.
 */
export function ProjectEditForm({
  project,
}: {
  project: ProjectEditForm_project$key;
}) {
  const data = useFragment(projectEditFormFragment, project);
  const [name, setName] = useState(data.name);
  const [clientName, setClientName] = useState(data.clientName);
  const [clientAbn, setClientAbn] = useState(data.clientAbn ?? "");
  const [clientAddress, setClientAddress] = useState(data.clientAddress ?? "");
  const [reference, setReference] = useState(data.reference ?? "");
  const [archived, setArchived] = useState(data.archived);
  const [billing, setBilling] = useState(() =>
    initialBillingValues({
      clientEmail: data.clientEmail,
      paymentTermsDays: data.paymentTermsDays,
      defaultUnitPriceCents: data.defaultUnitPriceCents,
    }),
  );
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  const [commit, isSaving] = useMutation<ProjectEditFormMutation>(graphql`
    mutation ProjectEditFormMutation($id: ID!, $input: UpdateProjectInput!) {
      updateProject(id: $id, input: $input) {
        ...ProjectEditForm_project
      }
    }
  `);

  function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (isSaving) return;
    setError(null);
    setSaved(false);
    const parsed = billingInput(billing);
    if (!parsed.ok) {
      setError(parsed.error);
      return;
    }
    commit({
      variables: {
        id: data.id,
        input: {
          name: name.trim(),
          clientName: clientName.trim(),
          clientAbn: clientAbn.trim(),
          clientAddress: clientAddress.trim(),
          reference: reference.trim(),
          ...parsed.input,
          archived,
        },
      },
      onCompleted: () => setSaved(true),
      onError: (err) =>
        setError(relayMutationErrorMessage(err, "Failed to save project.")),
    });
  }

  return (
    <Card>
      <h2 className="mb-4 text-sm font-semibold tracking-wide text-ink-muted uppercase">
        Details
      </h2>
      <form onSubmit={handleSubmit} className="flex flex-col gap-4">
        <FormField label="Project name" htmlFor="project-name">
          <TextInput
            id="project-name"
            value={name}
            onChange={(e) => setName(e.target.value)}
            maxLength={200}
            required
          />
        </FormField>
        <FormField label="Client name" htmlFor="project-client-name">
          <TextInput
            id="project-client-name"
            value={clientName}
            onChange={(e) => setClientName(e.target.value)}
            maxLength={200}
            required
          />
        </FormField>
        <FormField label="Client ABN" htmlFor="project-client-abn">
          <TextInput
            id="project-client-abn"
            value={clientAbn}
            onChange={(e) => setClientAbn(e.target.value)}
            maxLength={200}
          />
        </FormField>
        <FormField label="Client address" htmlFor="project-client-address">
          <textarea
            id="project-client-address"
            className={inputBase}
            rows={3}
            value={clientAddress}
            onChange={(e) => setClientAddress(e.target.value)}
            maxLength={2000}
          />
        </FormField>
        <FormField label="Reference" htmlFor="project-reference">
          <TextInput
            id="project-reference"
            value={reference}
            onChange={(e) => setReference(e.target.value)}
            maxLength={200}
            placeholder="e.g. the site address"
          />
        </FormField>
        <ProjectBillingFields
          idPrefix="project"
          values={billing}
          onChange={setBilling}
        />
        <label className="flex items-center gap-2 text-sm text-ink">
          <input
            type="checkbox"
            checked={archived}
            onChange={(e) => setArchived(e.target.checked)}
            className="size-4 rounded-sm border-line text-accent focus:ring-2 focus:ring-accent/25"
          />
          Archived
        </label>
        <p className="-mt-2 text-xs text-ink-muted">
          Archived projects are hidden from the projects list unless &quot;Show
          archived&quot; is ticked.
        </p>

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
