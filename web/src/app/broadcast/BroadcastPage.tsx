import { Suspense, useEffect, useState } from "react";
import { graphql } from "react-relay";
import type { BroadcastPageQuery } from "./__generated__/BroadcastPageQuery.graphql";
import type { BroadcastPageDraftQuery } from "./__generated__/BroadcastPageDraftQuery.graphql";
import { useRetryableLazyLoadQuery } from "../../components/useRetryableLazyLoadQuery";
import RelayErrorBoundary from "../../components/RelayErrorBoundary";
import LoadingIndicator from "../../components/LoadingIndicator";
import { useSelectedInstance } from "../SelectedInstanceContext";
import { useCurrentUser } from "../../auth/useCurrentUser";
import { Button } from "../../components/ui/Button";
import { formatTimestamp } from "../../lib/dates";
import type { BroadcastSender } from "../../lib/broadcast";
import { BroadcastForm } from "./BroadcastForm";
import { HtmlBroadcastEditor } from "./HtmlBroadcastEditor";
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
        id
        isDraft
        inProgress
        subject
        createdAt
        recipientCount
        lastTestSentAt
        ...BroadcastHistory_broadcasts
      }
    }
  }
`;
/* eslint-enable relay/unused-fields */

// The draft's fields are read by `HtmlBroadcastEditor`, reached through a
// prop rather than a fragment (it also edits a draft that isn't saved yet),
// so the lint rule can't see them.
/* eslint-disable relay/unused-fields */
const broadcastDraftQuery = graphql`
  query BroadcastPageDraftQuery($slug: String!, $id: ID!) @throwOnFieldError {
    instance(slug: $slug) {
      broadcast(id: $id) {
        id
        isDraft
        fromAddress
        fromName
        subject
        body
        html
        missingAssets
        lastTestSentAt
        recipients {
          email
        }
      }
    }
  }
`;
/* eslint-enable relay/unused-fields */

/** How often to refetch while a broadcast is still sending in the background. */
const PROGRESS_POLL_MS = 30_000;

type Mode = "plain" | "html";
/** What the HTML side shows: the drafts list, a new draft, or a saved one. */
type Editing = null | "new" | { id: string };

interface EditorProps {
  instanceId: string;
  defaultFromName: string;
  domains: readonly string[];
  senders: readonly BroadcastSender[];
  testEmail: string;
  onSaved: () => void;
  onSent: () => void;
  onClose: () => void;
}

/** Load a saved draft fresh, then edit it. */
function DraftEditor({
  slug,
  id,
  ...props
}: EditorProps & { slug: string; id: string }) {
  const data = useRetryableLazyLoadQuery<BroadcastPageDraftQuery>(
    broadcastDraftQuery,
    { slug, id },
    { fetchPolicy: "network-only" },
  );
  const draft = data.instance?.broadcast;
  if (!draft || !draft.isDraft) {
    return (
      <div className="rounded-lg border border-dashed border-line p-6 text-center text-ink-muted">
        This draft has been sent or deleted.{" "}
        <Button variant="ghost" onClick={props.onClose}>
          Back to drafts
        </Button>
      </div>
    );
  }
  return <HtmlBroadcastEditor {...props} draft={draft} />;
}

function Content({ slug }: { slug: string }) {
  const me = useCurrentUser();
  // Bumped after a send so the history below picks the new broadcast up, and
  // periodically while one is still sending.
  const [fetchKey, setFetchKey] = useState(0);
  const [mode, setMode] = useState<Mode>("plain");
  const [editing, setEditing] = useState<Editing>(null);
  const [sentNotice, setSentNotice] = useState(false);
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

  const instance = data.instance;
  const drafts = instance.broadcasts.filter((b) => b.isDraft);
  const sent = instance.broadcasts.filter((b) => !b.isDraft);
  const refetch = () => setFetchKey((k) => k + 1);
  const editorProps: EditorProps = {
    instanceId: instance.id,
    defaultFromName: instance.fromName,
    domains: instance.broadcastFromDomains,
    senders: instance.broadcastSenders,
    testEmail: me?.email ?? "",
    onSaved: refetch,
    onSent: () => {
      setEditing(null);
      setSentNotice(true);
      refetch();
    },
    onClose: () => setEditing(null),
  };

  return (
    <div className="flex flex-col gap-8">
      <div className="flex flex-col gap-4">
        <div
          role="radiogroup"
          aria-label="Broadcast format"
          className="flex gap-2"
        >
          {(
            [
              ["plain", "Plain text"],
              ["html", "HTML email"],
            ] as const
          ).map(([value, label]) => (
            <Button
              key={value}
              role="radio"
              aria-checked={mode === value}
              variant={mode === value ? "secondary" : "ghost"}
              onClick={() => setMode(value)}
            >
              {label}
            </Button>
          ))}
        </div>

        {mode === "plain" ? (
          <BroadcastForm
            instanceId={instance.id}
            defaultFromName={instance.fromName}
            domains={instance.broadcastFromDomains}
            senders={instance.broadcastSenders}
            onSent={refetch}
          />
        ) : editing === "new" ? (
          <HtmlBroadcastEditor {...editorProps} draft={null} />
        ) : editing ? (
          <Suspense fallback={<LoadingIndicator />}>
            <DraftEditor
              key={editing.id}
              slug={slug}
              id={editing.id}
              {...editorProps}
            />
          </Suspense>
        ) : (
          <section
            aria-label="Drafts"
            className="flex flex-col gap-3 rounded-lg border border-line p-4"
          >
            {sentNotice && (
              <p
                role="status"
                className="text-sm text-green-700 dark:text-green-400"
              >
                Queued. Progress shows in the history below.
              </p>
            )}
            <p className="text-sm text-ink-muted">
              Design the email anywhere — an email builder, or ask Claude — then
              paste its HTML or upload its export .zip. Toolbox hosts the
              images. Save it as a draft, send yourself a test, then send it.
            </p>
            {drafts.length > 0 && (
              <ul className="flex flex-col divide-y divide-line">
                {drafts.map((d) => (
                  <li
                    key={d.id}
                    className="flex items-center justify-between gap-2 py-2"
                  >
                    <div className="min-w-0">
                      <p className="truncate text-sm font-medium text-ink">
                        {d.subject}
                      </p>
                      <p className="text-xs text-ink-muted">
                        Draft · {d.recipientCount}{" "}
                        {d.recipientCount === 1 ? "recipient" : "recipients"} ·
                        started {formatTimestamp(d.createdAt)}
                        {d.lastTestSentAt != null && " · tested"}
                      </p>
                    </div>
                    <Button
                      variant="secondary"
                      onClick={() => {
                        setSentNotice(false);
                        setEditing({ id: d.id });
                      }}
                    >
                      Edit
                    </Button>
                  </li>
                ))}
              </ul>
            )}
            <Button
              className="self-start"
              onClick={() => {
                setSentNotice(false);
                setEditing("new");
              }}
            >
              New HTML broadcast
            </Button>
          </section>
        )}
      </div>
      <BroadcastSendersSection
        instanceId={instance.id}
        senders={instance.broadcastSenders}
        domains={instance.broadcastFromDomains}
      />
      <BroadcastHistory broadcasts={sent} />
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
