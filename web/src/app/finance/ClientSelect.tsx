import { graphql } from "react-relay";
import type { ClientSelectQuery } from "./__generated__/ClientSelectQuery.graphql";
import { useRetryableLazyLoadQuery } from "../../components/useRetryableLazyLoadQuery";
import { FormField } from "../../components/ui/FormField";
import { inputBase } from "../../components/ui/inputStyles";
import { Link } from "react-router";

const clientSelectQuery = graphql`
  query ClientSelectQuery($instanceId: ID!) @throwOnFieldError {
    clients(instanceId: $instanceId, includeArchived: true) {
      id
      name
      archived
    }
  }
`;

/**
 * Pick the client a project bills. Archived clients are left out, except
 * `current` — the project's own client, which stays choosable. Suspends on
 * first use; `store-and-network`, so a client just added on the clients
 * page appears.
 */
export function ClientSelect({
  instanceId,
  id,
  value,
  current,
  onChange,
}: {
  instanceId: string;
  id: string;
  value: string;
  current?: string | null;
  onChange: (clientId: string) => void;
}) {
  const data = useRetryableLazyLoadQuery<ClientSelectQuery>(
    clientSelectQuery,
    { instanceId },
    { fetchPolicy: "store-and-network" },
  );
  const choices = data.clients.filter((c) => !c.archived || c.id === current);
  return (
    <FormField label="Client" htmlFor={id}>
      <select
        id={id}
        className={inputBase}
        value={value}
        onChange={(e) => onChange(e.target.value)}
        required
      >
        <option value="" disabled>
          Choose a client…
        </option>
        {choices.map((c) => (
          <option key={c.id} value={c.id}>
            {c.name}
            {c.archived ? " (archived)" : ""}
          </option>
        ))}
      </select>
      <p className="mt-1 text-xs text-ink-muted">
        Who its invoices are made out to.{" "}
        <Link to="/app/clients">Add or edit clients</Link>.
      </p>
    </FormField>
  );
}
