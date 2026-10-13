import { Suspense, useState } from "react";
import { graphql } from "react-relay";
import type { BroadcastPageQuery } from "./__generated__/BroadcastPageQuery.graphql";
import { useRetryableLazyLoadQuery } from "../../components/useRetryableLazyLoadQuery";
import RelayErrorBoundary from "../../components/RelayErrorBoundary";
import LoadingIndicator from "../../components/LoadingIndicator";
import { useSelectedInstance } from "../SelectedInstanceContext";
import { BroadcastForm } from "./BroadcastForm";
import { BroadcastHistory } from "./BroadcastHistory";

const broadcastPageQuery = graphql`
  query BroadcastPageQuery($slug: String!) @throwOnFieldError {
    instance(slug: $slug) {
      id
      name
      fromName
      broadcastFromDomains
      broadcasts(first: 25) {
        ...BroadcastHistory_broadcasts
      }
    }
  }
`;

function Content({ slug }: { slug: string }) {
  // Bumped after a send so the history below picks the new broadcast up.
  const [fetchKey, setFetchKey] = useState(0);
  const data = useRetryableLazyLoadQuery<BroadcastPageQuery>(
    broadcastPageQuery,
    { slug },
    { fetchKey, fetchPolicy: "store-and-network" },
  );

  if (!data.instance) {
    return (
      <div className="rounded-lg border border-dashed border-line p-10 text-center text-ink-muted">
        Instance not found.
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-8">
      <BroadcastForm
        instanceId={data.instance.id}
        defaultFromName={data.instance.fromName}
        domains={data.instance.broadcastFromDomains}
        onSent={() => setFetchKey((k) => k + 1)}
      />
      <BroadcastHistory broadcasts={data.instance.broadcasts} />
    </div>
  );
}

/**
 * `/app/broadcast` — email an announcement to a list of addresses from the
 * selected support instance. Owner-only (`sendBroadcast` and
 * `Instance.broadcasts` are `InstanceOwner` in the API; the nav link is
 * hidden from agents as a convenience). Each recipient gets their own copy,
 * and each reply opens its own new ticket — see CLAUDE.md's "Broadcasts".
 */
export function BroadcastPage() {
  const instance = useSelectedInstance();

  if (!instance || instance.kind !== "SUPPORT") {
    return (
      <div className="rounded-lg border border-dashed border-line p-10 text-center text-ink-muted">
        Pick a support instance to send a broadcast from.
      </div>
    );
  }

  return (
    <div className="flex max-w-3xl flex-col gap-6">
      <div>
        <h1 className="text-xl font-semibold text-ink-strong">Broadcast</h1>
        <p className="mt-1 text-sm text-ink-muted">
          Email an announcement from {instance.name}. Each recipient gets their
          own copy, and every reply opens a new ticket.
        </p>
      </div>
      {instance.role === "OWNER" ? (
        <RelayErrorBoundary canRetry>
          <Suspense fallback={<LoadingIndicator />}>
            <Content key={instance.id} slug={instance.slug} />
          </Suspense>
        </RelayErrorBoundary>
      ) : (
        <div className="rounded-lg border border-dashed border-line p-10 text-center text-ink-muted">
          Only an owner of {instance.name} can send a broadcast.
        </div>
      )}
    </div>
  );
}
