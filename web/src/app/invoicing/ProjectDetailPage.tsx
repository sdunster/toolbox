import { Suspense } from "react";
import { graphql } from "react-relay";
import { useParams } from "react-router";
import type { ProjectDetailPageQuery } from "./__generated__/ProjectDetailPageQuery.graphql";
import { useRetryableLazyLoadQuery } from "../../components/useRetryableLazyLoadQuery";
import RelayErrorBoundary from "../../components/RelayErrorBoundary";
import LoadingIndicator from "../../components/LoadingIndicator";
import { ButtonLink } from "../../components/ui/Button";
import { RequireInvoicingInstance } from "./RequireInvoicingInstance";
import { ProjectEditForm } from "./ProjectEditForm";
import { ProjectBillableItems } from "./ProjectBillableItems";
import { ProjectInvoices } from "./ProjectInvoices";
import { ProjectExpenses } from "./ProjectExpenses";
import { ProjectFinancials } from "./ProjectFinancials";
import { centsToInput } from "../../lib/money";

const projectDetailPageQuery = graphql`
  query ProjectDetailPageQuery($id: ID!) @throwOnFieldError {
    project(id: $id) {
      id
      name
      clientName
      archived
      defaultUnitPriceCents
      instance {
        id
        invoicingSettings {
          currency
        }
      }
      ...ProjectEditForm_project
    }
  }
`;

function Content({ id }: { id: string }) {
  const data = useRetryableLazyLoadQuery<ProjectDetailPageQuery>(
    projectDetailPageQuery,
    { id },
  );

  if (!data.project) {
    return (
      <div className="rounded-lg border border-dashed border-line p-10 text-center text-ink-muted">
        <p className="font-medium text-ink">Project not found</p>
        <p className="mt-1 text-sm">
          It may not exist, or you may not have access to it.
        </p>
        <ButtonLink to="/app/projects" variant="secondary" className="mt-4">
          Back to projects
        </ButtonLink>
      </div>
    );
  }

  const project = data.project;
  return (
    <div className="flex max-w-6xl flex-col gap-6">
      <ButtonLink
        to="/app/projects"
        variant="ghost"
        className="self-start px-0"
      >
        ← Back to projects
      </ButtonLink>
      <div>
        <div className="flex flex-wrap items-center gap-2">
          <h1 className="text-xl font-semibold text-ink-strong">
            {project.name}
          </h1>
          {project.archived && (
            <span className="rounded-full bg-surface-sunken px-2 py-0.5 text-xs font-medium text-ink-muted">
              Archived
            </span>
          )}
        </div>
        <p className="mt-1 text-sm text-ink-muted">{project.clientName}</p>
      </div>
      <ProjectFinancials
        projectId={project.id}
        currency={project.instance.invoicingSettings?.currency ?? "AUD"}
      />
      <div className="max-w-3xl">
        <ProjectEditForm project={project} />
      </div>
      <ProjectBillableItems
        instanceId={project.instance.id}
        projectId={project.id}
        currency={project.instance.invoicingSettings?.currency ?? "AUD"}
        archived={project.archived}
        defaultPrice={
          project.defaultUnitPriceCents == null
            ? ""
            : centsToInput(project.defaultUnitPriceCents)
        }
      />
      <ProjectInvoices
        instanceId={project.instance.id}
        projectId={project.id}
      />
      <ProjectExpenses
        instanceId={project.instance.id}
        projectId={project.id}
        currency={project.instance.invoicingSettings?.currency ?? "AUD"}
        archived={project.archived}
      />
    </div>
  );
}

/**
 * `/app/projects/:id` — one project's details, editable in place. The
 * `project` query is `null` for a missing project and for one the caller
 * can't reach alike (see its doc comment), so both show the same "not
 * found" panel.
 */
export function ProjectDetailPage() {
  const { id } = useParams<{ id: string }>();
  if (!id) return null;

  return (
    <RequireInvoicingInstance purpose="see its projects">
      {() => (
        <RelayErrorBoundary canRetry>
          <Suspense fallback={<LoadingIndicator />}>
            <Content id={id} />
          </Suspense>
        </RelayErrorBoundary>
      )}
    </RequireInvoicingInstance>
  );
}
