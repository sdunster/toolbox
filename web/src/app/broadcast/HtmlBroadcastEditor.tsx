import { useId, useMemo, useRef, useState } from "react";
import { graphql, useMutation } from "react-relay";
import type {
  MutationParameters,
  Disposable,
  PayloadError,
} from "relay-runtime";
import type { UseMutationConfig } from "react-relay";
import type { HtmlBroadcastEditorSaveMutation } from "./__generated__/HtmlBroadcastEditorSaveMutation.graphql";
import type { HtmlBroadcastEditorTestMutation } from "./__generated__/HtmlBroadcastEditorTestMutation.graphql";
import type { HtmlBroadcastEditorSendMutation } from "./__generated__/HtmlBroadcastEditorSendMutation.graphql";
import type { HtmlBroadcastEditorDeleteMutation } from "./__generated__/HtmlBroadcastEditorDeleteMutation.graphql";
import type { HtmlBroadcastEditorUploadMutation } from "./__generated__/HtmlBroadcastEditorUploadMutation.graphql";
import { Button } from "../../components/ui/Button";
import { FormField } from "../../components/ui/FormField";
import TextInput from "../../components/ui/TextInput";
import { inputBase } from "../../components/ui/inputStyles";
import {
  BROADCAST_BATCH_INTERVAL_SECS,
  BROADCAST_BATCH_SIZE,
  MAX_BROADCAST_RECIPIENTS,
  estimatedSendSecs,
  formatDuration,
  formatSender,
  invalidRecipients,
  parseRecipients,
  type BroadcastSender,
} from "../../lib/broadcast";
import { formatTimestamp } from "../../lib/dates";
import {
  GMAIL_CLIP_BYTES,
  MAX_ASSET_BYTES,
  MAX_HTML_BYTES,
  byteLength,
  imageContentType,
  readEmailExport,
} from "../../lib/emailHtml";
import { relayMutationErrorMessage } from "../../lib/relayMutationError";
import { SenderFields } from "./SenderFields";
import {
  initialSenderChoice,
  resolveSender,
  senderChoiceFor,
} from "./senderChoice";

/** A saved draft as the editor opens it (`Instance.broadcast`). */
export interface BroadcastDraft {
  id: string;
  fromAddress: string;
  fromName: string;
  subject: string;
  body: string;
  html: string | null | undefined;
  missingAssets: readonly string[];
  lastTestSentAt: number | null | undefined;
  recipients: readonly { email: string }[];
}

type Saved = Pick<BroadcastDraft, "id" | "missingAssets" | "lastTestSentAt">;

const PREVIEW_WIDTHS = { Desktop: 640, Mobile: 375 } as const;

/** Run a Relay mutation as a promise. */
function run<T extends MutationParameters>(
  commit: (config: UseMutationConfig<T>) => Disposable,
  variables: T["variables"],
): Promise<T["response"]> {
  return new Promise((resolve, reject) => {
    commit({
      variables,
      onCompleted: (response, errors: PayloadError[] | null) => {
        if (errors?.length) reject(new Error(errors[0].message));
        else resolve(response);
      },
      onError: reject,
    });
  });
}

/**
 * Build an HTML broadcast as a draft, test it, then send it. The HTML is
 * made elsewhere (an email builder, MJML, or Claude) and pasted in, or
 * uploaded as an export `.zip`; every image it uses is uploaded to Toolbox
 * (`createBroadcastAssetUpload`), which hosts it permanently, and the API
 * rewrites the HTML to point at it (`saveBroadcastDraft`). "Send test"
 * mails a `[TEST]` copy to your own address; "Review & send" sends it for
 * real, exactly like a plain-text broadcast. The preview is a sandboxed
 * `<iframe>` — pasted HTML never runs as you.
 */
export function HtmlBroadcastEditor({
  instanceId,
  defaultFromName,
  domains,
  senders = [],
  testEmail,
  draft,
  onSaved,
  onSent,
  onClose,
}: {
  instanceId: string;
  defaultFromName: string;
  domains: readonly string[];
  senders?: readonly BroadcastSender[];
  /** Where a test goes by default — the viewer's own address. */
  testEmail: string;
  /** The draft to edit, or `null` to start a new one. */
  draft: BroadcastDraft | null;
  onSaved: () => void;
  onSent: () => void;
  onClose: () => void;
}) {
  const id = useId();
  const [senderChoice, setSenderChoice] = useState(() =>
    draft
      ? senderChoiceFor(draft.fromAddress, draft.fromName, senders, domains)
      : initialSenderChoice(senders, domains),
  );
  const [recipientsText, setRecipientsText] = useState(
    draft?.recipients.map((r) => r.email).join("\n") ?? "",
  );
  const [subject, setSubject] = useState(draft?.subject ?? "");
  const [html, setHtml] = useState(draft?.html ?? "");
  const [plainText, setPlainText] = useState(draft?.body ?? "");
  const [showPlainText, setShowPlainText] = useState(
    (draft?.body ?? "") !== "",
  );
  const [saved, setSaved] = useState<Saved | null>(draft);
  const [dirty, setDirty] = useState(false);
  const [testTo, setTestTo] = useState(testEmail);
  const [previewWidth, setPreviewWidth] =
    useState<keyof typeof PREVIEW_WIDTHS>("Desktop");
  const [busy, setBusy] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [reviewing, setReviewing] = useState(false);
  const [confirmingDelete, setConfirmingDelete] = useState(false);
  // Images uploaded from a .zip before the draft could first be saved; the
  // next save sends them.
  const pendingAssets = useRef<{ path: string; url: string }[]>([]);

  const [commitSave] = useMutation<HtmlBroadcastEditorSaveMutation>(graphql`
    mutation HtmlBroadcastEditorSaveMutation(
      $instanceId: ID!
      $input: SaveBroadcastDraftInput!
    ) {
      saveBroadcastDraft(instanceId: $instanceId, input: $input) {
        id
        html
        missingAssets
        lastTestSentAt
      }
    }
  `);
  const [commitTest] = useMutation<HtmlBroadcastEditorTestMutation>(graphql`
    mutation HtmlBroadcastEditorTestMutation(
      $instanceId: ID!
      $broadcastId: ID!
      $to: [String!]!
    ) {
      sendBroadcastTest(
        instanceId: $instanceId
        broadcastId: $broadcastId
        to: $to
      ) {
        id
        lastTestSentAt
      }
    }
  `);
  const [commitSend] = useMutation<HtmlBroadcastEditorSendMutation>(graphql`
    mutation HtmlBroadcastEditorSendMutation(
      $instanceId: ID!
      $broadcastId: ID!
    ) {
      sendBroadcastDraft(instanceId: $instanceId, broadcastId: $broadcastId) {
        id
        pendingCount
        recipientCount
      }
    }
  `);
  const [commitDelete] = useMutation<HtmlBroadcastEditorDeleteMutation>(graphql`
    mutation HtmlBroadcastEditorDeleteMutation(
      $instanceId: ID!
      $broadcastId: ID!
    ) {
      deleteBroadcastDraft(instanceId: $instanceId, broadcastId: $broadcastId)
    }
  `);
  const [commitUpload] = useMutation<HtmlBroadcastEditorUploadMutation>(graphql`
    mutation HtmlBroadcastEditorUploadMutation(
      $instanceId: ID!
      $filename: String!
      $contentType: String!
      $size: Int!
    ) {
      createBroadcastAssetUpload(
        instanceId: $instanceId
        filename: $filename
        contentType: $contentType
        size: $size
      ) {
        uploadUrl
        url
      }
    }
  `);

  const recipients = useMemo(
    () => parseRecipients(recipientsText),
    [recipientsText],
  );
  const invalid = invalidRecipients(recipients);
  const tooMany = recipients.length > MAX_BROADCAST_RECIPIENTS;
  const sender = resolveSender(senderChoice, senders);
  const htmlBytes = byteLength(html);
  const canSave =
    sender.complete &&
    subject.trim() !== "" &&
    html.trim() !== "" &&
    htmlBytes <= MAX_HTML_BYTES &&
    invalid.length === 0 &&
    !tooMany &&
    busy === null;
  const missing = saved?.missingAssets ?? [];
  const ready = canSave && missing.length === 0;
  const testedThisVersion = !dirty && saved?.lastTestSentAt != null;

  function edit<T>(set: (value: T) => void) {
    return (value: T) => {
      set(value);
      setDirty(true);
      setNotice(null);
    };
  }

  function takePendingAssets() {
    const assets = pendingAssets.current;
    pendingAssets.current = [];
    return assets;
  }

  /** Save the draft as it stands, plus any newly uploaded images. */
  async function save(
    assetUrls: { path: string; url: string }[] = [],
    htmlToSave = html,
  ) {
    const response = await run(commitSave, {
      instanceId,
      input: {
        id: saved?.id ?? null,
        fromAddress: sender.fromAddress,
        fromName: sender.fromName || null,
        subject: subject.trim(),
        body: showPlainText ? plainText : "",
        html: htmlToSave,
        assetUrls,
        recipients,
      },
    });
    const draft = response.saveBroadcastDraft;
    // Show what will actually be sent: images at their hosted URLs, and
    // anything a mail client would strip already gone.
    setHtml(draft.html ?? "");
    setSaved(draft);
    setDirty(false);
    onSaved();
    return draft;
  }

  /** Upload one image; returns the path → URL pair to save with. */
  async function upload(path: string, file: File) {
    const contentType = imageContentType(file.name) ?? file.type;
    if (!imageContentType(file.name)) {
      throw new Error(`${file.name}: use a PNG, JPEG, GIF or WebP image.`);
    }
    if (file.size > MAX_ASSET_BYTES) {
      throw new Error(`${file.name} is over 5 MB.`);
    }
    const response = await run(commitUpload, {
      instanceId,
      filename: file.name,
      contentType,
      size: file.size,
    });
    const target = response.createBroadcastAssetUpload;
    const res = await fetch(target.uploadUrl, {
      method: "PUT",
      headers: { "Content-Type": contentType },
      body: file,
    });
    if (!res.ok)
      throw new Error(`Uploading ${file.name} failed (HTTP ${res.status}).`);
    return { path, url: target.url };
  }

  /** Run `task` with a busy label, reporting failure in the error line. */
  async function step(
    label: string,
    fallback: string,
    task: () => Promise<void>,
  ) {
    setBusy(label);
    setError(null);
    setNotice(null);
    try {
      await task();
    } catch (err) {
      setError(relayMutationErrorMessage(err, fallback));
    } finally {
      setBusy(null);
    }
  }

  function importZip(file: File) {
    void step("Reading the .zip…", "Couldn't import the .zip.", async () => {
      const exported = await readEmailExport(file);
      setHtml(exported.html);
      setDirty(true);
      const assets = [];
      for (const [i, image] of exported.images.entries()) {
        setBusy(`Uploading image ${i + 1} of ${exported.images.length}…`);
        assets.push(await upload(image.path, image.file));
      }
      if (!sender.complete || subject.trim() === "") {
        setNotice(
          `Imported ${exported.images.length} image${exported.images.length === 1 ? "" : "s"}. Add a sender and subject, then save.`,
        );
        // Not saved yet, so remember the uploads for the first save.
        pendingAssets.current.push(...assets);
        return;
      }
      setBusy("Saving…");
      await save([...takePendingAssets(), ...assets], exported.html);
      setNotice("Imported and saved.");
    });
  }

  function uploadMissing(path: string, file: File) {
    void step(
      `Uploading ${file.name}…`,
      "Couldn't upload the image.",
      async () => {
        const asset = await upload(path, file);
        setBusy("Saving…");
        await save([...takePendingAssets(), asset]);
      },
    );
  }

  function saveNow() {
    void step("Saving…", "Couldn't save the draft.", async () => {
      await save(takePendingAssets());
      setNotice("Draft saved.");
    });
  }

  function sendTest() {
    void step("Sending test…", "Couldn't send the test.", async () => {
      const current = dirty || !saved ? await save(takePendingAssets()) : saved;
      if (current.missingAssets.length > 0) {
        throw new Error("Upload the missing images first.");
      }
      const to = parseRecipients(testTo);
      const response = await run(commitTest, {
        instanceId,
        broadcastId: current.id,
        to,
      });
      setSaved({ ...current, ...response.sendBroadcastTest });
      setNotice(
        `Test sent to ${to.join(", ")}. Check how it looks before sending it for real.`,
      );
    });
  }

  function sendForReal() {
    void step(
      "Sending…",
      "Couldn't send the broadcast. Nothing was sent — try again.",
      async () => {
        const current =
          dirty || !saved ? await save(takePendingAssets()) : saved;
        await run(commitSend, { instanceId, broadcastId: current.id });
        setReviewing(false);
        onSent();
      },
    );
  }

  function deleteDraft() {
    if (!saved) {
      onClose();
      return;
    }
    void step("Deleting…", "Couldn't delete the draft.", async () => {
      await run(commitDelete, { instanceId, broadcastId: saved.id });
      onSaved();
      onClose();
    });
  }

  if (domains.length === 0) {
    return (
      <div className="rounded-lg border border-dashed border-line p-10 text-center text-ink-muted">
        This instance has no inbound address, so there is no domain to send a
        broadcast from. Add one first.
      </div>
    );
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
            {formatSender({
              name: sender.fromName || defaultFromName,
              address: sender.fromAddress,
            })}
          </dd>
          <dt className="text-ink-muted">Subject</dt>
          <dd className="text-ink">{subject.trim()}</dd>
          <dt className="text-ink-muted">To</dt>
          <dd className="break-all text-ink">{recipients.join(", ")}</dd>
          <dt className="text-ink-muted">Tested</dt>
          <dd className="text-ink">
            {testedThisVersion && saved?.lastTestSentAt != null
              ? formatTimestamp(saved.lastTestSentAt)
              : "Not tested since it was last changed"}
          </dd>
        </dl>
        {!testedThisVersion && (
          <p className="text-sm text-amber-700 dark:text-amber-400">
            You haven&apos;t sent yourself a test of this version. It&apos;s
            worth checking how it looks in a real inbox first.
          </p>
        )}
        <p className="text-sm text-ink-muted">
          This can&apos;t be undone. Each person gets their own copy.{" "}
          {recipients.length > BROADCAST_BATCH_SIZE
            ? `It goes out ${BROADCAST_BATCH_SIZE} at a time, every ${BROADCAST_BATCH_INTERVAL_SECS / 60} minutes — about ${formatDuration(estimatedSendSecs(recipients.length))} in all.`
            : "It goes out within a minute or so."}
        </p>
        {error && (
          <p role="alert" className="text-sm text-red-600 dark:text-red-400">
            {error}
          </p>
        )}
        <div className="flex gap-2">
          <Button onClick={sendForReal} disabled={busy !== null}>
            {busy ?? "Send now"}
          </Button>
          <Button
            variant="ghost"
            onClick={() => setReviewing(false)}
            disabled={busy !== null}
          >
            Back
          </Button>
        </div>
      </section>
    );
  }

  return (
    <section
      aria-label="HTML broadcast"
      className="flex flex-col gap-4 rounded-lg border border-line p-4"
    >
      <div className="flex items-baseline justify-between gap-2">
        <h2 className="text-base font-semibold text-ink-strong">
          {saved ? "Edit draft" : "New HTML broadcast"}
        </h2>
        <Button variant="ghost" onClick={onClose} disabled={busy !== null}>
          Close
        </Button>
      </div>

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
          onChange={(e) => edit(setRecipientsText)(e.target.value)}
        />
        <p className="text-xs text-ink-muted">
          One per line, or separated by commas. {recipients.length}{" "}
          {recipients.length === 1 ? "address" : "addresses"}. You can save the
          draft before adding them.
        </p>
      </FormField>

      <SenderFields
        id={id}
        choice={senderChoice}
        onChange={edit(setSenderChoice)}
        senders={senders}
        domains={domains}
        defaultFromName={defaultFromName}
      />

      <FormField label="Subject" htmlFor={`${id}-subject`}>
        <TextInput
          id={`${id}-subject`}
          value={subject}
          onChange={(e) => edit(setSubject)(e.target.value)}
        />
      </FormField>

      <FormField
        label="HTML"
        htmlFor={`${id}-html`}
        error={
          htmlBytes > MAX_HTML_BYTES
            ? `Too large: at most ${MAX_HTML_BYTES / 1000} KB.`
            : null
        }
      >
        <textarea
          id={`${id}-html`}
          className={`${inputBase} min-h-48 font-mono text-xs`}
          placeholder="<!DOCTYPE html>…"
          spellCheck={false}
          value={html}
          onChange={(e) => edit(setHtml)(e.target.value)}
        />
        <div className="flex flex-wrap items-center gap-x-4 gap-y-1 text-xs text-ink-muted">
          <span>
            Paste the email&apos;s HTML, or{" "}
            <label className="cursor-pointer text-accent underline">
              upload an export .zip
              <input
                type="file"
                accept=".zip,application/zip"
                className="sr-only"
                disabled={busy !== null}
                onChange={(e) => {
                  const file = e.target.files?.[0];
                  e.target.value = "";
                  if (file) importZip(file);
                }}
              />
            </label>{" "}
            (index.html plus its images).
          </span>
          <span
            className={
              htmlBytes > GMAIL_CLIP_BYTES
                ? "text-amber-700 dark:text-amber-400"
                : undefined
            }
          >
            {Math.ceil(htmlBytes / 1000)} KB
            {htmlBytes > GMAIL_CLIP_BYTES &&
              " — Gmail may clip it behind “View entire message”"}
          </span>
        </div>
      </FormField>

      {missing.length > 0 && (
        <div className="flex flex-col gap-2 rounded-md border border-amber-300 bg-amber-50 p-3 text-sm dark:border-amber-800 dark:bg-amber-950/40">
          <p className="font-medium text-ink">
            Upload {missing.length === 1 ? "this image" : "these images"} so
            Toolbox can host {missing.length === 1 ? "it" : "them"}:
          </p>
          <ul className="flex flex-col gap-1">
            {missing.map((path) => (
              <li key={path} className="flex flex-wrap items-center gap-2">
                <code className="text-xs">{path}</code>
                <input
                  type="file"
                  aria-label={`Upload ${path}`}
                  accept="image/png,image/jpeg,image/gif,image/webp"
                  className="text-xs"
                  disabled={busy !== null}
                  onChange={(e) => {
                    const file = e.target.files?.[0];
                    e.target.value = "";
                    if (file) uploadMissing(path, file);
                  }}
                />
              </li>
            ))}
          </ul>
        </div>
      )}

      <div className="flex flex-col gap-1.5">
        <label className="flex items-center gap-2 text-sm text-ink">
          <input
            type="checkbox"
            checked={showPlainText}
            onChange={(e) => edit(setShowPlainText)(e.target.checked)}
          />
          Write my own plain-text version
        </label>
        {showPlainText ? (
          <textarea
            aria-label="Plain-text version"
            className={`${inputBase} min-h-28`}
            value={plainText}
            onChange={(e) => edit(setPlainText)(e.target.value)}
          />
        ) : (
          <p className="text-xs text-ink-muted">
            Otherwise one is made from the HTML, for mail apps that don&apos;t
            show HTML. No signature is added to an HTML broadcast.
          </p>
        )}
      </div>

      {html.trim() !== "" && (
        <div className="flex flex-col gap-2">
          <div className="flex items-center gap-2 text-sm">
            <span className="font-medium text-ink">Preview</span>
            {(
              Object.keys(PREVIEW_WIDTHS) as (keyof typeof PREVIEW_WIDTHS)[]
            ).map((w) => (
              <Button
                key={w}
                variant={w === previewWidth ? "secondary" : "ghost"}
                onClick={() => setPreviewWidth(w)}
              >
                {w}
              </Button>
            ))}
          </div>
          {/* sandbox="" — no scripts, no same-origin access, no forms:
              pasted HTML can never act as the logged-in user. */}
          <iframe
            title="Email preview"
            sandbox=""
            srcDoc={html}
            className="h-128 w-full rounded-md border border-line bg-white"
            style={{ maxWidth: PREVIEW_WIDTHS[previewWidth] }}
          />
          <p className="text-xs text-ink-muted">
            A browser preview. Mail apps differ — send yourself a test to see
            the real thing.
          </p>
        </div>
      )}

      {error && (
        <p role="alert" className="text-sm text-red-600 dark:text-red-400">
          {error}
        </p>
      )}
      {notice && (
        <p role="status" className="text-sm text-green-700 dark:text-green-400">
          {notice}
        </p>
      )}
      {busy && <p className="text-sm text-ink-muted">{busy}</p>}

      <div className="flex flex-wrap items-end gap-2">
        <Button variant="secondary" onClick={saveNow} disabled={!canSave}>
          Save draft
        </Button>
        <div className="flex items-end gap-2">
          <FormField label="Send a test to" htmlFor={`${id}-test-to`}>
            <TextInput
              id={`${id}-test-to`}
              value={testTo}
              onChange={(e) => setTestTo(e.target.value)}
            />
          </FormField>
          <Button
            variant="secondary"
            onClick={sendTest}
            disabled={!ready || parseRecipients(testTo).length === 0}
          >
            Send test
          </Button>
        </div>
        <Button
          onClick={() => {
            setError(null);
            setReviewing(true);
          }}
          disabled={!ready || recipients.length === 0}
        >
          Review &amp; send
        </Button>
      </div>
      {saved?.lastTestSentAt != null && (
        <p className="text-xs text-ink-muted">
          {testedThisVersion
            ? `Test sent ${formatTimestamp(saved.lastTestSentAt)}.`
            : "Changed since the last test."}
        </p>
      )}

      <div className="border-t border-line pt-3">
        {confirmingDelete ? (
          <div className="flex items-center gap-2 text-sm">
            <span className="text-ink">Delete this draft?</span>
            <Button
              variant="danger"
              onClick={deleteDraft}
              disabled={busy !== null}
            >
              Delete
            </Button>
            <Button variant="ghost" onClick={() => setConfirmingDelete(false)}>
              Keep it
            </Button>
          </div>
        ) : (
          <Button variant="ghost" onClick={() => setConfirmingDelete(true)}>
            {saved ? "Delete draft" : "Discard"}
          </Button>
        )}
      </div>
    </section>
  );
}
