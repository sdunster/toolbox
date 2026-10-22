import { Suspense, useDeferredValue, useState } from "react";
import { graphql } from "react-relay";
import { Link } from "react-router";
import type { ProjectListPageQuery } from "./__generated__/ProjectListPageQuery.graphql";
import { useRetryableLazyLoadQuery } from "../../components/useRetryableLazyLoadQuery";
import RelayErrorBoundary from "../../components/RelayErrorBoundary";
import LoadingIndicator from "../../components/LoadingIndicator";
import { RequireFinanceInstance } from "./RequireFinanceInstance";
import { CreateProjectForm } from "./CreateProjectForm";

const projectListPageQuery = graphql`
  query ProjectListPageQuery($instanceId: ID!, $includeArchived: Boolean!)
  @throwOnFieldError {
    projects(instanceId: $instanceId, includeArchived: $includeArchived) {
      id
      name
      client {
        name
      }
      reference
      archived
    }
  }
`;

function ProjectList({
  instanceId,
  includeArchived,
}: {
  instanceId: string;
  includeArchived: boolean;
}) {
  // `store-and-network`: `projects` is a plain list, not a `@connection`, so
  // a project created (or archived) elsewhere isn't spliced into a cached
  // copy of it — coming back to this page shows the cached list instantly
  // and then refreshes it.
  const data = useRetryableLazyLoadQuery<ProjectListPageQuery>(
    projectListPageQuery,
    { instanceId, includeArchived },
    { fetchPolicy: "store-and-network" },
  );

  if (data.projects.length === 0) {
    return (
      <p className="text-sm text-ink-muted">
        {includeArchived
          ? "No projects yet."
          : "No active projects — create one below."}
      </p>
    );
  }

  return (
    <ul className="flex flex-col divide-y divide-line-faint rounded-lg border border-line">
      {data.projects.map((project) => (
        <li key={project.id}>
          <Link
            to={`/app/projects/${project.id}`}
            className="flex flex-wrap items-center justify-between gap-2 p-3 no-underline transition-colors hover:bg-surface-raised"
          >
            <div className="flex flex-col">
              <div className="flex items-center gap-2">
                <span className="font-medium text-ink-strong">
                  {project.name}
                </span>
                {project.archived && (
                  <span className="rounded-full bg-surface-sunken px-2 py-0.5 text-xs font-medium text-ink-muted">
                    Archived
                  </span>
                )}
              </div>
              <span className="text-sm text-ink-muted">
                {project.client.name}
              </span>
            </div>
            {project.reference && (
              <span className="text-sm text-ink-muted">
                {project.reference}
              </span>
            )}
          </Link>
        </li>
      ))}
    </ul>
  );
}

function Content({ instanceId, slug }: { instanceId: string; slug: string }) {
  const [includeArchived, setIncludeArchived] = useState(false);
  // Deferred, so flipping the toggle updates the checkbox immediately but
  // keeps showing the current list (dimmed) while the other variant loads,
  // instead of flashing the Suspense fallback.
  const deferredIncludeArchived = useDeferredValue(includeArchived);
  const isPending = deferredIncludeArchived !== includeArchived;

  return (
    <div className="flex max-w-3xl flex-col gap-8">
      <div className="flex flex-wrap items-end justify-between gap-3">
        <h1 className="text-xl font-semibold text-ink-strong">Projects</h1>
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
          <ProjectList
            instanceId={instanceId}
            includeArchived={deferredIncludeArchived}
          />
        </Suspense>
      </div>

      <CreateProjectForm instanceId={instanceId} slug={slug} />
    </div>
  );
}

/**
 * `/app/projects` — the selected finance instance's projects (jobs, each
 * billed to one client), sorted by name, plus the create form. Archived projects are hidden
 * unless "Show archived" is ticked, which re-runs `projects` with
 * `includeArchived: true`.
 */
export function ProjectListPage() {
  return (
    <RequireFinanceInstance purpose="see its projects">
      {(instance) => (
        <RelayErrorBoundary canRetry>
          <Suspense fallback={<LoadingIndicator />}>
            <Content
              key={instance.id}
              instanceId={instance.id}
              slug={instance.slug}
            />
          </Suspense>
        </RelayErrorBoundary>
      )}
    </RequireFinanceInstance>
  );
}
