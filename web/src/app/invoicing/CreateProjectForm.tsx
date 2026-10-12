import { useState } from "react";
import { graphql, useMutation } from "react-relay";
import { useNavigate } from "react-router";
import type { CreateProjectFormMutation } from "./__generated__/CreateProjectFormMutation.graphql";
import { relayMutationErrorMessage } from "../../lib/relayMutationError";
import { Card } from "../../components/ui/Card";
import { Button } from "../../components/ui/Button";
import { FormField } from "../../components/ui/FormField";
import TextInput from "../../components/ui/TextInput";
import { inputBase } from "../../components/ui/inputStyles";
import { ProjectBillingFields } from "./ProjectBillingFields";
import { billingInput, initialBillingValues } from "../../lib/projectBilling";

/**
 * Create a project in the selected invoicing instance, then go straight to
 * its page — where its billable items and invoices will live. The list page
 * refetches on return (see `ProjectListPage`'s fetch policy), so no store
 * updater is needed here. Blank optional fields are sent as-is; the API
 * trims them and omits the attribute.
 */
export function CreateProjectForm({ instanceId }: { instanceId: string }) {
  const navigate = useNavigate();
  const [name, setName] = useState("");
  const [clientName, setClientName] = useState("");
  const [clientAbn, setClientAbn] = useState("");
  const [clientAddress, setClientAddress] = useState("");
  const [reference, setReference] = useState("");
  const [billing, setBilling] = useState(() => initialBillingValues());
  const [error, setError] = useState<string | null>(null);

  const [commit, isSaving] = useMutation<CreateProjectFormMutation>(graphql`
    mutation CreateProjectFormMutation(
      $instanceId: ID!
      $input: CreateProjectInput!
    ) {
      createProject(instanceId: $instanceId, input: $input) {
        id
      }
    }
  `);

  function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (isSaving) return;
    setError(null);
    const parsed = billingInput(billing);
    if (!parsed.ok) {
      setError(parsed.error);
      return;
    }
    commit({
      variables: {
        instanceId,
        input: {
          name: name.trim(),
          clientName: clientName.trim(),
          clientAbn: clientAbn.trim(),
          clientAddress: clientAddress.trim(),
          reference: reference.trim(),
          ...parsed.input,
        },
      },
      onCompleted: (data) => {
        navigate(`/app/projects/${data.createProject.id}`);
      },
      onError: (err) =>
        setError(relayMutationErrorMessage(err, "Failed to create project.")),
    });
  }

  return (
    <Card>
      <h2 className="mb-4 text-sm font-semibold tracking-wide text-ink-muted uppercase">
        New project
      </h2>
      <form onSubmit={handleSubmit} className="flex flex-col gap-4">
        <FormField label="Project name" htmlFor="new-project-name">
          <TextInput
            id="new-project-name"
            value={name}
            onChange={(e) => setName(e.target.value)}
            maxLength={200}
            required
          />
        </FormField>
        <FormField label="Client name" htmlFor="new-project-client-name">
          <TextInput
            id="new-project-client-name"
            value={clientName}
            onChange={(e) => setClientName(e.target.value)}
            maxLength={200}
            required
          />
        </FormField>
        <FormField label="Client ABN" htmlFor="new-project-client-abn">
          <TextInput
            id="new-project-client-abn"
            value={clientAbn}
            onChange={(e) => setClientAbn(e.target.value)}
            maxLength={200}
          />
        </FormField>
        <FormField label="Client address" htmlFor="new-project-client-address">
          <textarea
            id="new-project-client-address"
            className={inputBase}
            rows={3}
            value={clientAddress}
            onChange={(e) => setClientAddress(e.target.value)}
            maxLength={2000}
          />
        </FormField>
        <FormField label="Reference" htmlFor="new-project-reference">
          <TextInput
            id="new-project-reference"
            value={reference}
            onChange={(e) => setReference(e.target.value)}
            maxLength={200}
            placeholder="e.g. the site address"
          />
        </FormField>
        <ProjectBillingFields
          idPrefix="new-project"
          values={billing}
          onChange={setBilling}
        />

        {error && (
          <p role="alert" className="text-sm text-red-600 dark:text-red-400">
            {error}
          </p>
        )}

        <Button type="submit" disabled={isSaving} className="self-start">
          {isSaving ? "Creating…" : "Create project"}
        </Button>
      </form>
    </Card>
  );
}
