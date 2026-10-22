import { Suspense, useDeferredValue, useState } from "react";
import { graphql, useFragment, useMutation } from "react-relay";
import { Link } from "react-router";
import type { ClientListPageQuery } from "./__generated__/ClientListPageQuery.graphql";
import type { ClientListPage_client$key } from "./__generated__/ClientListPage_client.graphql";
import type { ClientListPageUpdateMutation } from "./__generated__/ClientListPageUpdateMutation.graphql";
import type { ClientListPageCreateMutation } from "./__generated__/ClientListPageCreateMutation.graphql";
import { useRetryableLazyLoadQuery } from "../../components/useRetryableLazyLoadQuery";
import RelayErrorBoundary from "../../components/RelayErrorBoundary";
import LoadingIndicator from "../../components/LoadingIndicator";
import { Card } from "../../components/ui/Card";
import { Button } from "../../components/ui/Button";
import { relayMutationErrorMessage } from "../../lib/relayMutationError";
import { clientInput, initialClientValues } from "../../lib/clients";
import { RequireFinanceInstance } from "./RequireFinanceInstance";
import { ClientFields } from "./ClientFields";

const clientListPageQuery = graphql`
  query ClientListPageQuery($instanceId: ID!, $includeArchived: Boolean!)
  @throwOnFieldError {
    clients(instanceId: $instanceId, includeArchived: $includeArchived) {
      id
      ...ClientListPage_client
    }
  }
`;

// `abn`/`address`/`paymentTermsDays` are read through
// `initialClientValues(data)`, which the lint rule can't follow.
/* eslint-disable relay/unused-fields */
const clientFragment = graphql`
  fragment ClientListPage_client on Client {
    id
    name
    abn
    address
    email
    paymentTermsDays
    effectivePaymentTermsDays
    archived
    projects {
      id
      name
    }
  }
`;
/* eslint-enable relay/unused-fields */

/**
 * One client: its details, its projects, and an edit form behind "Edit".
 * `updateClient` returns the `Client` by `id`, so Relay merges the result
 * into the list with no updater.
 */
function ClientRow({ client }: { client: ClientListPage_client$key }) {
  const data = useFragment(clientFragment, client);
  const [editing, setEditing] = useState(false);
  const [values, setValues] = useState(() => initialClientValues(data));
  const [archived, setArchived] = useState(data.archived);
  const [error, setError] = useState<string | null>(null);

  const [commit, isSaving] = useMutation<ClientListPageUpdateMutation>(graphql`
    mutation ClientListPageUpdateMutation(
      $id: ID!
      $input: ClientInput!
      $archived: Boolean!
    ) {
      updateClient(id: $id, input: $input, archived: $archived) {
        ...ClientListPage_client
      }
    }
  `);

  function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (isSaving) return;
    setError(null);
    const parsed = clientInput(values);
    if (!parsed.ok) {
      setError(parsed.error);
      return;
    }
    commit({
      variables: { id: data.id, input: parsed.input, archived },
      onCompleted: () => setEditing(false),
      onError: (err) =>
        setError(relayMutationErrorMessage(err, "Failed to save client.")),
    });
  }

  return (
    <li className="flex flex-col gap-3 p-3">
      <div className="flex flex-wrap items-start justify-between gap-2">
        <div className="flex flex-col">
          <div className="flex items-center gap-2">
            <span className="font-medium text-ink-strong">{data.name}</span>
            {data.archived && (
              <span className="rounded-full bg-surface-sunken px-2 py-0.5 text-xs font-medium text-ink-muted">
                Archived
              </span>
            )}
          </div>
          <span className="text-sm text-ink-muted">
            {[data.email, `${data.effectivePaymentTermsDays}-day terms`]
              .filter(Boolean)
              .join(" · ")}
          </span>
          {data.projects.length > 0 && (
            <span className="text-sm text-ink-muted">
              Projects:{" "}
              {data.projects.map((p, i) => (
                <span key={p.id}>
                  {i > 0 && ", "}
                  <Link to={`/app/projects/${p.id}`}>{p.name}</Link>
                </span>
              ))}
            </span>
          )}
        </div>
        <Button
          variant="secondary"
          onClick={() => {
            setValues(initialClientValues(data));
            setArchived(data.archived);
            setError(null);
            setEditing(!editing);
          }}
        >
          {editing ? "Cancel" : "Edit"}
        </Button>
      </div>
      {editing && (
        <form onSubmit={handleSubmit} className="flex flex-col gap-4">
          <ClientFields
            idPrefix={`client-${data.id}`}
            values={values}
            onChange={setValues}
          />
          <label className="flex items-center gap-2 text-sm text-ink">
            <input
              type="checkbox"
              checked={archived}
              onChange={(e) => setArchived(e.target.checked)}
              className="size-4 rounded-sm border-line text-accent focus:ring-2 focus:ring-accent/25"
            />
            Archived (hidden from new projects)
          </label>
          <p className="-mt-2 text-xs text-ink-muted">
            Changes reach its projects&apos; draft invoices; finalized invoices
            keep what they were issued with.
          </p>
          {error && (
            <p role="alert" className="text-sm text-red-600 dark:text-red-400">
              {error}
            </p>
          )}
          <Button type="submit" disabled={isSaving} className="self-start">
            {isSaving ? "Saving…" : "Save"}
          </Button>
        </form>
      )}
    </li>
  );
}

function ClientList({
  instanceId,
  includeArchived,
}: {
  instanceId: string;
  includeArchived: boolean;
}) {
  // `store-and-network`, as on the projects page: `clients` is a plain list.
  const data = useRetryableLazyLoadQuery<ClientListPageQuery>(
    clientListPageQuery,
    { instanceId, includeArchived },
    { fetchPolicy: "store-and-network" },
  );
  if (data.clients.length === 0) {
    return (
      <p className="text-sm text-ink-muted">No clients yet — add one below.</p>
    );
  }
  return (
    <ul className="flex flex-col divide-y divide-line-faint rounded-lg border border-line">
      {data.clients.map((client) => (
        <ClientRow key={client.id} client={client} />
      ))}
    </ul>
  );
}

function CreateClientForm({
  instanceId,
  onCreated,
}: {
  instanceId: string;
  onCreated: () => void;
}) {
  const [values, setValues] = useState(() => initialClientValues());
  const [error, setError] = useState<string | null>(null);
  const [commit, isSaving] = useMutation<ClientListPageCreateMutation>(graphql`
    mutation ClientListPageCreateMutation(
      $instanceId: ID!
      $input: ClientInput!
    ) {
      createClient(instanceId: $instanceId, input: $input) {
        id
      }
    }
  `);

  function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (isSaving) return;
    setError(null);
    const parsed = clientInput(values);
    if (!parsed.ok) {
      setError(parsed.error);
      return;
    }
    commit({
      variables: { instanceId, input: parsed.input },
      onCompleted: () => {
        setValues(initialClientValues());
        onCreated();
      },
      onError: (err) =>
        setError(relayMutationErrorMessage(err, "Failed to create client.")),
    });
  }

  return (
    <Card>
      <h2 className="mb-4 text-sm font-semibold tracking-wide text-ink-muted uppercase">
        New client
      </h2>
      <form onSubmit={handleSubmit} className="flex flex-col gap-4">
        <ClientFields
          idPrefix="new-client"
          values={values}
          onChange={setValues}
        />
        {error && (
          <p role="alert" className="text-sm text-red-600 dark:text-red-400">
            {error}
          </p>
        )}
        <Button type="submit" disabled={isSaving} className="self-start">
          {isSaving ? "Creating…" : "Create client"}
        </Button>
      </form>
    </Card>
  );
}

function Content({ instanceId }: { instanceId: string }) {
  const [includeArchived, setIncludeArchived] = useState(false);
  // Bumped after a create, so the list (a plain query, not a connection)
  // re-mounts and refetches.
  const [version, setVersion] = useState(0);
  const deferredIncludeArchived = useDeferredValue(includeArchived);
  const isPending = deferredIncludeArchived !== includeArchived;

  return (
    <div className="flex max-w-3xl flex-col gap-8">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <h1 className="text-xl font-semibold text-ink-strong">Clients</h1>
        <label className="flex items-center gap-2 text-sm text-ink">
          <input
            type="checkbox"
            checked={includeArchived}
            onChange={(e) => setIncludeArchived(e.target.checked)}
            className="size-4 rounded-sm border-line text-accent focus:ring-2 focus:ring-accent/25"
          />
          Show archived
        </label>
      </div>
      <div className={isPending ? "opacity-60" : undefined}>
        <Suspense fallback={<LoadingIndicator />}>
          <ClientList
            key={version}
            instanceId={instanceId}
            includeArchived={deferredIncludeArchived}
          />
        </Suspense>
      </div>
      <CreateClientForm
        instanceId={instanceId}
        onCreated={() => setVersion((v) => v + 1)}
      />
    </div>
  );
}

/**
 * `/app/clients` — who the selected finance instance bills: each client's
 * "Invoice to" details, email and payment terms, and its projects. Every
 * project belongs to one client.
 */
export function ClientListPage() {
  return (
    <RequireFinanceInstance purpose="see its clients">
      {(instance) => (
        <RelayErrorBoundary canRetry>
          <Suspense fallback={<LoadingIndicator />}>
            <Content key={instance.id} instanceId={instance.id} />
          </Suspense>
        </RelayErrorBoundary>
      )}
    </RequireFinanceInstance>
  );
}
