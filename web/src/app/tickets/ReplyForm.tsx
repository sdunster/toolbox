import { useState } from "react";
import { graphql, useMutation } from "react-relay";
import type { RecordSourceSelectorProxy } from "relay-runtime";
import type { ReplyFormMutation } from "./__generated__/ReplyFormMutation.graphql";
import type { ReplyFormCreateUploadMutation } from "./__generated__/ReplyFormCreateUploadMutation.graphql";
import { Button } from "../../components/ui/Button";
import {
  relayMutationErrorCode,
  relayMutationErrorMessage,
} from "../../lib/relayMutationError";

interface PendingAttachment {
  id: string;
  filename: string;
  status: "uploading" | "done" | "error";
  key?: string;
}

/**
 * Reply to the customer. Always sends mail (`replyToTicket`'s primary
 * effect) — see `NoteForm` for the internal-note counterpart, deliberately
 * a *separate* component/box rather than a toggle on this one, per the
 * build plan.
 *
 * The result (a persisted `TicketMessage`, possibly without a
 * `rfcMessageId` if the send failed) isn't predictable ahead of time, so
 * this has no `optimisticResponse` — only an explicit `updater` that
 * appends the real response onto `ticket.messages`, which is a plain list
 * field (not a Relay connection), so `@appendEdge` doesn't apply here; this
 * is the "explicit record update" the build plan calls out as the
 * alternative.
 */
export function ReplyForm({ ticketId }: { ticketId: string }) {
  const [body, setBody] = useState("");
  const [attachments, setAttachments] = useState<PendingAttachment[]>([]);
  const [error, setError] = useState<string | null>(null);

  const [commitUpload] = useMutation<ReplyFormCreateUploadMutation>(graphql`
    mutation ReplyFormCreateUploadMutation(
      $ticketId: ID!
      $filename: String!
      $contentType: String!
    ) {
      createAttachmentUpload(
        ticketId: $ticketId
        filename: $filename
        contentType: $contentType
      ) {
        key
        uploadUrl
      }
    }
  `);

  const [commitReply, isReplying] = useMutation<ReplyFormMutation>(graphql`
    mutation ReplyFormMutation(
      $ticketId: ID!
      $body: String!
      $attachmentKeys: [String!]
    ) {
      replyToTicket(
        ticketId: $ticketId
        body: $body
        attachmentKeys: $attachmentKeys
      ) {
        id
        ...MessageItem_message
      }
    }
  `);

  async function handleFilesSelected(files: FileList | null) {
    if (!files || files.length === 0) return;
    for (const file of Array.from(files)) {
      const pendingId = `${file.name}-${Date.now()}-${Math.random()}`;
      setAttachments((prev) => [
        ...prev,
        { id: pendingId, filename: file.name, status: "uploading" },
      ]);
      try {
        const key = await new Promise<string>((resolve, reject) => {
          commitUpload({
            variables: {
              ticketId,
              filename: file.name,
              contentType: file.type || "application/octet-stream",
            },
            onCompleted: (response) => {
              const upload = response.createAttachmentUpload;
              fetch(upload.uploadUrl, {
                method: "PUT",
                headers: {
                  "Content-Type": file.type || "application/octet-stream",
                },
                body: file,
              })
                .then((res) => {
                  if (!res.ok)
                    throw new Error(`Upload failed: HTTP ${res.status}`);
                  resolve(upload.key);
                })
                .catch(reject);
            },
            onError: reject,
          });
        });
        setAttachments((prev) =>
          prev.map((a) =>
            a.id === pendingId ? { ...a, status: "done", key } : a,
          ),
        );
      } catch {
        setAttachments((prev) =>
          prev.map((a) => (a.id === pendingId ? { ...a, status: "error" } : a)),
        );
      }
    }
  }

  function removeAttachment(id: string) {
    setAttachments((prev) => prev.filter((a) => a.id !== id));
  }

  const uploading = attachments.some((a) => a.status === "uploading");
  const canSubmit = body.trim().length > 0 && !uploading && !isReplying;

  function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (!canSubmit) return;
    setError(null);
    const attachmentKeys = attachments
      .filter((a) => a.status === "done" && a.key)
      .map((a) => a.key as string);

    const updater = (store: RecordSourceSelectorProxy) => {
      const ticketRecord = store.get(ticketId);
      const newMessage = store.getRootField("replyToTicket");
      if (!ticketRecord || !newMessage) return;
      const existing = ticketRecord.getLinkedRecords("messages") ?? [];
      ticketRecord.setLinkedRecords([...existing, newMessage], "messages");
    };

    commitReply({
      variables: { ticketId, body, attachmentKeys },
      updater,
      onCompleted: () => {
        setBody("");
        setAttachments([]);
      },
      // CONFLICT is the API refusing a reply nobody could receive (every
      // recipient is undeliverable — see `replyToTicket`): its message names
      // them, which is what the agent needs. Anything else stays generic.
      onError: (err) =>
        setError(
          relayMutationErrorCode(err) === "CONFLICT"
            ? `${relayMutationErrorMessage(err, "Reply refused.")} Your draft is unchanged.`
            : "Failed to send reply. Your draft is unchanged — try again.",
        ),
    });
  }

  return (
    <form
      onSubmit={handleSubmit}
      className="flex flex-col gap-3 rounded-lg border border-line bg-surface p-4"
    >
      <h3 className="text-sm font-semibold text-ink-strong">
        Reply to customer
      </h3>
      <textarea
        aria-label="Reply to customer"
        className="min-h-28 w-full rounded-md border border-line bg-surface px-3 py-2 text-sm text-ink focus:border-accent focus:ring-2 focus:ring-accent/25 focus:outline-none"
        placeholder="Write a reply — this will be emailed to the requester and CCs."
        value={body}
        onChange={(e) => setBody(e.target.value)}
      />

      <div className="flex flex-col gap-2">
        <label className="text-sm text-ink-muted">
          Attach files
          <input
            type="file"
            multiple
            className="mt-1 block w-full text-sm text-ink"
            onChange={(e) => {
              void handleFilesSelected(e.target.files);
              e.target.value = "";
            }}
          />
        </label>
        {attachments.length > 0 && (
          <ul className="flex flex-col gap-1 text-sm">
            {attachments.map((a) => (
              <li key={a.id} className="flex items-center gap-2 text-ink-muted">
                <span>{a.filename}</span>
                <span>
                  {a.status === "uploading" && "uploading…"}
                  {a.status === "done" && "uploaded"}
                  {a.status === "error" && (
                    <span className="text-red-600 dark:text-red-400">
                      upload failed
                    </span>
                  )}
                </span>
                <button
                  type="button"
                  className="cursor-pointer text-xs underline"
                  onClick={() => removeAttachment(a.id)}
                >
                  Remove
                </button>
              </li>
            ))}
          </ul>
        )}
      </div>

      {error && (
        <p role="alert" className="text-sm text-red-600 dark:text-red-400">
          {error}
        </p>
      )}

      <Button type="submit" disabled={!canSubmit} className="self-start">
        {isReplying ? "Sending…" : "Send reply"}
      </Button>
    </form>
  );
}
