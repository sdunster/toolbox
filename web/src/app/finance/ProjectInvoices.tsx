import { Suspense } from "react";
import { graphql } from "react-relay";
import type { ProjectInvoicesQuery } from "./__generated__/ProjectInvoicesQuery.graphql";
import { useRetryableLazyLoadQuery } from "../../components/useRetryableLazyLoadQuery";
import RelayErrorBoundary from "../../components/RelayErrorBoundary";
import LoadingIndicator from "../../components/LoadingIndicator";
import { InvoiceList } from "./InvoiceList";

const projectInvoicesQuery = graphql`
  query ProjectInvoicesQuery($instanceId: ID!, $projectId: ID!)
  @throwOnFieldError {
    ...InvoiceList_query
      @arguments(instanceId: $instanceId, projectId: $projectId, filter: ALL)
  }
`;

function Invoices({
  instanceId,
  projectId,
}: {
  instanceId: string;
  projectId: string;
}) {
  // `store-and-network`, same reasoning as `InvoiceListPage`/`ProjectListPage`.
  const data = useRetryableLazyLoadQuery<ProjectInvoicesQuery>(
    projectInvoicesQuery,
    { instanceId, projectId },
    { fetchPolicy: "store-and-network" },
  );
  return (
    <InvoiceList
      query={data}
      filter="ALL"
      showProject={false}
      emptyMessage="No invoices on this project yet — select some unbilled items above to start one."
    />
  );
}

/**
 * The project page's "Invoices" section: every invoice created from this
 * project's items, newest first. `instanceId` is the *project's* instance,
 * same reasoning as `ProjectBillableItems`.
 */
export function ProjectInvoices({
  instanceId,
  projectId,
}: {
  instanceId: string;
  projectId: string;
}) {
  return (
    <section
      aria-labelledby="project-invoices-heading"
      className="flex flex-col gap-4"
    >
      <h2
        id="project-invoices-heading"
        className="text-lg font-semibold text-ink-strong"
      >
        Invoices
      </h2>
      <RelayErrorBoundary canRetry>
        <Suspense fallback={<LoadingIndicator />}>
          <Invoices instanceId={instanceId} projectId={projectId} />
        </Suspense>
      </RelayErrorBoundary>
    </section>
  );
}
