import { useState } from "react";
import { graphql, useMutation } from "react-relay";
import { useNavigate } from "react-router";
import type { CreateProjectFormMutation } from "./__generated__/CreateProjectFormMutation.graphql";
import { relayMutationErrorMessage } from "../../lib/relayMutationError";
import { Card } from "../../components/ui/Card";
import { Button } from "../../components/ui/Button";
import { FormField } from "../../components/ui/FormField";
import TextInput from "../../components/ui/TextInput";
import { ProjectBillingFields } from "./ProjectBillingFields";
import { billingInput, initialBillingValues } from "../../lib/projectBilling";
import { DEFAULT_BRAND_ID } from "../../lib/brands";
import { InstanceBrandSelect } from "./BrandSelect";
import { ClientSelect } from "./ClientSelect";

/**
 * Create a project in the selected finance instance, then go straight to
 * its page — where its billable items and invoices will live. The list page
 * refetches on return (see `ProjectListPage`'s fetch policy), so no store
 * updater is needed here. Blank optional fields are sent as-is; the API
 * trims them and omits the attribute.
 */
export function CreateProjectForm({
  instanceId,
  slug,
}: {
  instanceId: string;
  slug: string;
}) {
  const navigate = useNavigate();
  const [name, setName] = useState("");
  const [clientId, setClientId] = useState("");
  const [reference, setReference] = useState("");
  const [billing, setBilling] = useState(() => initialBillingValues());
  const [brandId, setBrandId] = useState(DEFAULT_BRAND_ID);
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
          clientId,
          name: name.trim(),
          reference: reference.trim(),
          ...parsed.input,
          brandId,
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
        <ClientSelect
          instanceId={instanceId}
          id="new-project-client"
          value={clientId}
          onChange={setClientId}
        />
        <FormField label="Reference" htmlFor="new-project-reference">
          <TextInput
            id="new-project-reference"
            value={reference}
            onChange={(e) => setReference(e.target.value)}
            maxLength={200}
            placeholder="e.g. the site address"
          />
          <p className="mt-1 text-xs text-ink-muted">
            Printed on its invoices, unless an invoice has its own (a PO
            number).
          </p>
        </FormField>
        <InstanceBrandSelect
          slug={slug}
          id="new-project-brand"
          value={brandId}
          onChange={setBrandId}
          help="The name this project's invoices are issued under."
        />
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
