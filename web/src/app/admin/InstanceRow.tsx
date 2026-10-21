import { graphql, useFragment } from "react-relay";
import { Link } from "react-router";
import type { InstanceRow_instance$key } from "./__generated__/InstanceRow_instance.graphql";

const instanceRowFragment = graphql`
  fragment InstanceRow_instance on Instance {
    id
    name
    slug
    kind
    publicSubmissionEnabled
    deleted
  }
`;

/**
 * One row of the admin instance list, badged with its kind (the public
 * submission status only applies to a support instance, so a finance
 * row leaves it off). `deleted` is surfaced as a badge, not
 * filtered out — `adminInstances` deliberately includes soft-deleted
 * instances so this list is also where one gets restored (see that query's
 * doc comment).
 */
export function InstanceRow({
  instance,
}: {
  instance: InstanceRow_instance$key;
}) {
  const data = useFragment(instanceRowFragment, instance);

  return (
    <Link
      to={`/app/admin/instances/${data.id}`}
      className="flex flex-wrap items-center justify-between gap-2 p-3 no-underline transition-colors hover:bg-surface-raised"
    >
      <div className="flex items-center gap-2">
        <span className="font-medium text-ink-strong">{data.name}</span>
        <span className="text-sm text-ink-muted">/{data.slug}</span>
        <span className="rounded-full bg-surface-sunken px-2 py-0.5 text-xs font-medium text-ink-muted">
          {data.kind === "FINANCE" ? "Finance" : "Support"}
        </span>
        {data.deleted && (
          <span className="rounded-full bg-red-100 px-2 py-0.5 text-xs font-medium text-red-800 dark:bg-red-900/40 dark:text-red-300">
            Deleted
          </span>
        )}
      </div>
      {data.kind !== "FINANCE" && (
        <span className="text-sm text-ink-muted">
          {data.publicSubmissionEnabled
            ? "Public submission on"
            : "Public submission off"}
        </span>
      )}
    </Link>
  );
}
