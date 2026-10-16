import { Suspense, useEffect, useState } from "react";
import { graphql } from "react-relay";
import type { BroadcastPageQuery } from "./__generated__/BroadcastPageQuery.graphql";
import { useRetryableLazyLoadQuery } from "../../components/useRetryableLazyLoadQuery";
import RelayErrorBoundary from "../../components/RelayErrorBoundary";
import LoadingIndicator from "../../components/LoadingIndicator";
import { useSelectedInstance } from "../SelectedInstanceContext";
import { BroadcastForm } from "./BroadcastForm";
import { BroadcastHistory } from "./BroadcastHistory";
import { BroadcastSendersSection } from "./BroadcastSendersSection";

// `broadcastSenders { name address }` is read by `BroadcastForm` and
// `BroadcastSendersSection`, reached through props rather than a fragment, so
// the lint rule can't see it.
/* eslint-disable relay/unused-fields */
const broadcastPageQuery = graphql`
  query BroadcastPageQuery($slug: String!) @throwOnFieldError {
    instance(slug: $slug) {
      id
      name
      fromName
      broadcastFromDomains
      broadcastSenders {
        id
        name
        address
      }
      broadcasts(first: 25) {
        inProgress
        ...BroadcastHistory_broadcasts
      }
    }
  }
`;
/* eslint-enable relay/unused-fields */

/** How often to refetch while a broadcast is still sending in the background. */
const PROGRESS_POLL_MS = 30_000;

function Content({ slug }: { slug: string }) {
  // Bumped after a send so the history below picks the new broadcast up, and
  // periodically while one is still sending.
  const [fetchKey, setFetchKey] = useState(0);
  const data = useRetryableLazyLoadQuery<BroadcastPageQuery>(
    broadcastPageQuery,
    { slug },
    { fetchKey, fetchPolicy: "store-and-network" },
  );

  // A broadcast goes out in batches over minutes or hours: keep its progress
  // current while one is sending, and stop once none is.
  const sending = data.instance?.broadcasts.some((b) => b.inProgress) ?? false;
  useEffect(() => {
    if (!sending) return;
    const timer = setInterval(
      () => setFetchKey((k) => k + 1),
      PROGRESS_POLL_MS,
    );
    return () => clearInterval(timer);
  }, [sending]);

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
        senders={data.instance.broadcastSenders}
        onSent={() => setFetchKey((k) => k + 1)}
      />
      <BroadcastSendersSection
        instanceId={data.instance.id}
        senders={data.instance.broadcastSenders}
        domains={data.instance.broadcastFromDomains}
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
