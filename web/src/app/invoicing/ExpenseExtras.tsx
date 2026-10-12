import { useRef, useState } from "react";
import { graphql, useMutation } from "react-relay";
import type { ExpenseExtrasRebillMutation } from "./__generated__/ExpenseExtrasRebillMutation.graphql";
import type { ExpenseExtrasUploadMutation } from "./__generated__/ExpenseExtrasUploadMutation.graphql";
import type { ExpenseExtrasAttachMutation } from "./__generated__/ExpenseExtrasAttachMutation.graphql";
import type { ExpenseExtrasRemoveMutation } from "./__generated__/ExpenseExtrasRemoveMutation.graphql";
import type { ExpenseExtrasDownloadMutation } from "./__generated__/ExpenseExtrasDownloadMutation.graphql";
import { relayMutationErrorMessage } from "../../lib/relayMutationError";
import { tw } from "../../lib/tw";

const linkButton = tw`cursor-pointer text-xs font-medium text-accent hover:underline disabled:opacity-60`;

/**
 * An expense's receipt and re-billing controls, under its details: attach
 * or download a receipt file, and — for an expense on a project — pass the
 * cost on to the client as a billable item (`rebillExpense`), optionally
 * marked up. A re-billed expense says so, and can't be re-billed again
 * until that item is deleted.
 */
export function ExpenseExtras({
  expenseId,
  hasProject,
  rebilled,
  receiptFilename,
  onChanged,
}: {
  expenseId: string;
  hasProject: boolean;
  rebilled: boolean;
  receiptFilename: string | null;
  onChanged: () => void;
}) {
  const [error, setError] = useState<string | null>(null);
  const [rebilling, setRebilling] = useState(false);
  const [markup, setMarkup] = useState("");
  const [uploading, setUploading] = useState(false);
  const fileInput = useRef<HTMLInputElement>(null);

  const [commitRebill, isRebilling] = useMutation<ExpenseExtrasRebillMutation>(
    graphql`
      mutation ExpenseExtrasRebillMutation($id: ID!, $markup: String) {
        rebillExpense(expenseId: $id, markupPercent: $markup) {
          id
          sourceExpense {
            id
            ...ExpenseRow_expense
          }
        }
      }
    `,
  );
  const [commitUpload] = useMutation<ExpenseExtrasUploadMutation>(graphql`
    mutation ExpenseExtrasUploadMutation(
      $id: ID!
      $filename: String!
      $contentType: String!
    ) {
      createExpenseReceiptUpload(
        expenseId: $id
        filename: $filename
        contentType: $contentType
      ) {
        key
        uploadUrl
      }
    }
  `);
  const [commitAttach] = useMutation<ExpenseExtrasAttachMutation>(graphql`
    mutation ExpenseExtrasAttachMutation(
      $id: ID!
      $key: String!
      $contentType: String!
    ) {
      attachExpenseReceipt(
        expenseId: $id
        key: $key
        contentType: $contentType
      ) {
        ...ExpenseRow_expense
      }
    }
  `);
  const [commitRemove, isRemoving] = useMutation<ExpenseExtrasRemoveMutation>(
    graphql`
      mutation ExpenseExtrasRemoveMutation($id: ID!) {
        removeExpenseReceipt(expenseId: $id) {
          ...ExpenseRow_expense
        }
      }
    `,
  );
  const [commitDownload, isDownloading] =
    useMutation<ExpenseExtrasDownloadMutation>(graphql`
      mutation ExpenseExtrasDownloadMutation($id: ID!) {
        downloadExpenseReceipt(expenseId: $id)
      }
    `);

  async function upload(file: File) {
    const contentType = file.type || "application/octet-stream";
    setError(null);
    setUploading(true);
    try {
      const key = await new Promise<string>((resolve, reject) => {
        commitUpload({
          variables: { id: expenseId, filename: file.name, contentType },
          onCompleted: (response) => {
            const target = response.createExpenseReceiptUpload;
            fetch(target.uploadUrl, {
              method: "PUT",
              headers: { "Content-Type": contentType },
              body: file,
            })
              .then((res) => {
                if (!res.ok) throw new Error(`HTTP ${res.status}`);
                resolve(target.key);
              })
              .catch(reject);
          },
          onError: reject,
        });
      });
      await new Promise<void>((resolve, reject) => {
        commitAttach({
          variables: { id: expenseId, key, contentType },
          onCompleted: () => resolve(),
          onError: reject,
        });
      });
    } catch (err) {
      setError(
        err instanceof Error
          ? relayMutationErrorMessage(err, "Failed to upload the receipt.")
          : "Failed to upload the receipt.",
      );
    } finally {
      setUploading(false);
      if (fileInput.current) fileInput.current.value = "";
    }
  }

  return (
    <div className="mt-1 flex flex-col gap-1">
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
        {receiptFilename ? (
          <>
            <button
              type="button"
              className={linkButton}
              disabled={isDownloading}
              onClick={() =>
                commitDownload({
                  variables: { id: expenseId },
                  onCompleted: (d) =>
                    window.location.assign(d.downloadExpenseReceipt),
                  onError: (err) =>
                    setError(
                      relayMutationErrorMessage(err, "Failed to download."),
                    ),
                })
              }
            >
              Receipt: {receiptFilename}
            </button>
            <button
              type="button"
              className={`${linkButton} text-ink-muted`}
              disabled={isRemoving}
              onClick={() =>
                commitRemove({
                  variables: { id: expenseId },
                  onError: (err) =>
                    setError(
                      relayMutationErrorMessage(err, "Failed to remove."),
                    ),
                })
              }
            >
              Remove
            </button>
          </>
        ) : (
          <label className={`${linkButton} ${uploading ? "opacity-60" : ""}`}>
            {uploading ? "Uploading…" : "Attach receipt"}
            <input
              ref={fileInput}
              type="file"
              className="sr-only"
              disabled={uploading}
              accept="image/*,application/pdf"
              onChange={(e) => {
                const file = e.target.files?.[0];
                if (file) void upload(file);
              }}
            />
          </label>
        )}
        {rebilled ? (
          <span className="text-xs text-ink-muted">Re-billed to client</span>
        ) : (
          hasProject &&
          !rebilling && (
            <button
              type="button"
              className={linkButton}
              onClick={() => setRebilling(true)}
            >
              Re-bill to client
            </button>
          )
        )}
      </div>
      {rebilling && !rebilled && (
        <form
          className="flex flex-wrap items-center gap-2"
          onSubmit={(e) => {
            e.preventDefault();
            setError(null);
            commitRebill({
              variables: { id: expenseId, markup: markup.trim() || null },
              onCompleted: () => {
                setRebilling(false);
                onChanged();
              },
              onError: (err) =>
                setError(relayMutationErrorMessage(err, "Failed to re-bill.")),
            });
          }}
        >
          <label
            className="text-xs text-ink-muted"
            htmlFor={`markup-${expenseId}`}
          >
            Markup %
          </label>
          <input
            id={`markup-${expenseId}`}
            inputMode="decimal"
            className="w-16 rounded-sm border border-line bg-surface px-1.5 py-0.5 text-xs"
            value={markup}
            onChange={(e) => setMarkup(e.target.value)}
            placeholder="0"
          />
          <button type="submit" className={linkButton} disabled={isRebilling}>
            {isRebilling ? "Adding…" : "Add billable item"}
          </button>
          <button
            type="button"
            className={`${linkButton} text-ink-muted`}
            onClick={() => setRebilling(false)}
          >
            Cancel
          </button>
        </form>
      )}
      {error && (
        <p role="alert" className="text-xs text-red-600 dark:text-red-400">
          {error}
        </p>
      )}
    </div>
  );
}
