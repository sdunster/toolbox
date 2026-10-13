import { useId, useMemo, useState } from "react";
import { graphql, useMutation } from "react-relay";
import type {
  BroadcastFormMutation,
  BroadcastFormMutation$data,
} from "./__generated__/BroadcastFormMutation.graphql";
import { Button } from "../../components/ui/Button";
import { FormField } from "../../components/ui/FormField";
import TextInput from "../../components/ui/TextInput";
import { inputBase } from "../../components/ui/inputStyles";
import {
  MAX_BROADCAST_RECIPIENTS,
  invalidRecipients,
  parseRecipients,
} from "../../lib/broadcast";
import { relayMutationErrorMessage } from "../../lib/relayMutationError";

type SentBroadcast = BroadcastFormMutation$data["sendBroadcast"];

/**
 * Compose and send a broadcast (`sendBroadcast`). Sending is irreversible,
 * so "Review" first shows exactly who it will go to and from where, and
 * only "Send now" on that step actually sends.
 */
export function BroadcastForm({
  instanceId,
  defaultFromName,
  domains,
  onSent,
}: {
  instanceId: string;
  defaultFromName: string;
  domains: readonly string[];
  onSent: () => void;
}) {
  const id = useId();
  const [recipientsText, setRecipientsText] = useState("");
  const [localPart, setLocalPart] = useState("");
  const [domain, setDomain] = useState(domains[0] ?? "");
  const [fromName, setFromName] = useState("");
  const [subject, setSubject] = useState("");
  const [body, setBody] = useState("");
  const [reviewing, setReviewing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [sent, setSent] = useState<SentBroadcast | null>(null);

  const [commit, isSending] = useMutation<BroadcastFormMutation>(graphql`
    mutation BroadcastFormMutation(
      $instanceId: ID!
      $input: SendBroadcastInput!
    ) {
      sendBroadcast(instanceId: $instanceId, input: $input) {
        id
        subject
        fromAddress
        recipientCount
        sentCount
        recipients {
          email
          sendStatus
          error
        }
      }
    }
  `);

  const recipients = useMemo(
    () => parseRecipients(recipientsText),
    [recipientsText],
  );
  const invalid = invalidRecipients(recipients);
  const fromAddress = `${localPart.trim().toLowerCase()}@${domain}`;
  const tooMany = recipients.length > MAX_BROADCAST_RECIPIENTS;
  const canReview =
    recipients.length > 0 &&
    !tooMany &&
    invalid.length === 0 &&
    localPart.trim() !== "" &&
    domain !== "" &&
    subject.trim() !== "" &&
    body.trim() !== "";

  if (domains.length === 0) {
    return (
      <div className="rounded-lg border border-dashed border-line p-10 text-center text-ink-muted">
        This instance has no inbound address, so there is no domain to send a
        broadcast from. Add one first.
      </div>
    );
  }

  function send() {
    setError(null);
    commit({
      variables: {
        instanceId,
        input: {
          fromAddress,
          fromName: fromName.trim() || null,
          subject: subject.trim(),
          body,
          recipients,
        },
      },
      onCompleted: (data) => {
        setSent(data.sendBroadcast);
        setReviewing(false);
        setRecipientsText("");
        setSubject("");
        setBody("");
        onSent();
      },
      onError: (err) => {
        setReviewing(false);
        setError(
          relayMutationErrorMessage(
            err,
            "Couldn't send the broadcast. Nothing was sent — try again.",
          ),
        );
      },
    });
  }

  if (reviewing) {
    return (
      <section
        aria-label="Review broadcast"
        className="flex flex-col gap-4 rounded-lg border border-line p-4"
      >
        <h2 className="text-base font-semibold text-ink-strong">
          Send to {recipients.length}{" "}
          {recipients.length === 1 ? "recipient" : "recipients"}?
        </h2>
        <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1 text-sm">
          <dt className="text-ink-muted">From</dt>
          <dd className="text-ink">
            {(fromName.trim() || defaultFromName) + ` <${fromAddress}>`}
          </dd>
          <dt className="text-ink-muted">Subject</dt>
          <dd className="text-ink">{subject.trim()}</dd>
          <dt className="text-ink-muted">To</dt>
          <dd className="break-all text-ink">{recipients.join(", ")}</dd>
        </dl>
        <p className="text-sm text-ink-muted">
          This can&apos;t be undone. Each person gets their own copy.
        </p>
        <div className="flex gap-2">
          <Button onClick={send} disabled={isSending}>
            {isSending ? "Sending…" : "Send now"}
          </Button>
          <Button
            variant="ghost"
            onClick={() => setReviewing(false)}
            disabled={isSending}
          >
            Back
          </Button>
        </div>
      </section>
    );
  }

  return (
    <div className="flex flex-col gap-4">
      {sent && <SentSummary sent={sent} onDismiss={() => setSent(null)} />}
      <form
        className="flex flex-col gap-4 rounded-lg border border-line p-4"
        onSubmit={(e) => {
          e.preventDefault();
          if (canReview) setReviewing(true);
        }}
      >
        <FormField
          label="Recipients"
          htmlFor={`${id}-recipients`}
          error={
            tooMany
              ? `At most ${MAX_BROADCAST_RECIPIENTS} recipients per broadcast.`
              : invalid.length > 0
                ? `Not an email address: ${invalid.join(", ")}`
                : null
          }
        >
          <textarea
            id={`${id}-recipients`}
            className={`${inputBase} min-h-28 font-mono`}
            placeholder={"one@example.com\ntwo@example.com"}
            value={recipientsText}
            onChange={(e) => setRecipientsText(e.target.value)}
          />
          <p className="text-xs text-ink-muted">
            One per line, or separated by commas. {recipients.length}{" "}
            {recipients.length === 1 ? "address" : "addresses"}.
          </p>
        </FormField>

        <div className="grid gap-4 sm:grid-cols-2">
          <FormField label="From address" htmlFor={`${id}-from`}>
            <div className="flex items-center gap-1">
              <TextInput
                id={`${id}-from`}
                placeholder="news"
                value={localPart}
                onChange={(e) => setLocalPart(e.target.value)}
              />
              <span className="text-sm text-ink-muted">@</span>
              {domains.length === 1 ? (
                <span className="text-sm text-ink">{domain}</span>
              ) : (
                <select
                  aria-label="From domain"
                  className={inputBase}
                  value={domain}
                  onChange={(e) => setDomain(e.target.value)}
                >
                  {domains.map((d) => (
                    <option key={d} value={d}>
                      {d}
                    </option>
                  ))}
                </select>
              )}
            </div>
          </FormField>
          <FormField label="From name" htmlFor={`${id}-from-name`}>
            <TextInput
              id={`${id}-from-name`}
              placeholder={defaultFromName}
              value={fromName}
              onChange={(e) => setFromName(e.target.value)}
            />
          </FormField>
        </div>

        <FormField label="Subject" htmlFor={`${id}-subject`}>
          <TextInput
            id={`${id}-subject`}
            value={subject}
            onChange={(e) => setSubject(e.target.value)}
          />
        </FormField>

        <FormField label="Message" htmlFor={`${id}-body`}>
          <textarea
            id={`${id}-body`}
            className={`${inputBase} min-h-48`}
            value={body}
            onChange={(e) => setBody(e.target.value)}
          />
          <p className="text-xs text-ink-muted">
            Plain text. The instance signature is added below it.
          </p>
        </FormField>

        {error && (
          <p role="alert" className="text-sm text-red-600 dark:text-red-400">
            {error}
          </p>
        )}
        <Button type="submit" className="self-start" disabled={!canReview}>
          Review
        </Button>
      </form>
    </div>
  );
}

function SentSummary({
  sent,
  onDismiss,
}: {
  sent: SentBroadcast;
  onDismiss: () => void;
}) {
  // Failed at send time, or skipped because the address is suppressed (a
  // past hard bounce or spam complaint) — `error` says which.
  const notSent = sent.recipients.filter(
    (r) => r.sendStatus === "FAILED" || r.sendStatus === "SUPPRESSED",
  );
  return (
    <div
      role="status"
      className="flex flex-col gap-2 rounded-lg border border-green-300 bg-green-50 p-4 text-sm dark:border-green-800 dark:bg-green-950/40"
    >
      <p className="font-medium text-green-800 dark:text-green-300">
        Sent “{sent.subject}” to {sent.sentCount} of {sent.recipientCount}{" "}
        {sent.recipientCount === 1 ? "recipient" : "recipients"}.
      </p>
      {notSent.length > 0 && (
        <ul className="list-disc pl-5 text-red-700 dark:text-red-400">
          {notSent.map((r) => (
            <li key={r.email}>
              {r.sendStatus === "SUPPRESSED"
                ? `Skipped ${r.error ?? r.email} — it bounced or complained before.`
                : `${r.email}: ${r.error ?? "failed to send"}`}
            </li>
          ))}
        </ul>
      )}
      <p className="text-ink-muted">
        Bounces and spam complaints show up in the history below as they arrive.
      </p>
      <Button variant="ghost" className="self-start" onClick={onDismiss}>
        Dismiss
      </Button>
    </div>
  );
}
