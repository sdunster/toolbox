import { useState } from "react";
import { Button } from "../../components/ui/Button";
import { FormField } from "../../components/ui/FormField";
import TextInput from "../../components/ui/TextInput";
import { inputBase } from "../../components/ui/inputStyles";

export interface SendDocumentValues {
  to: string[];
  cc: string[];
  message: string | null;
}

/** Split a comma/semicolon/space-separated address list. */
function splitAddresses(raw: string): string[] {
  return raw
    .split(/[\s,;]+/)
    .map((a) => a.trim())
    .filter((a) => a.length > 0);
}

/**
 * The recipients and covering note for emailing an invoice or credit note
 * to the client. Blank "To" means the project's client email
 * (`clientEmail`, shown as the placeholder). Emailing can't be undone, so
 * the button says plainly what it does.
 */
export function SendDocumentForm({
  idPrefix,
  clientEmail,
  isSending,
  error,
  submitLabel,
  onSubmit,
  onCancel,
}: {
  idPrefix: string;
  clientEmail: string | null;
  isSending: boolean;
  error: string | null;
  submitLabel: string;
  onSubmit: (values: SendDocumentValues) => void;
  onCancel: () => void;
}) {
  const [to, setTo] = useState("");
  const [cc, setCc] = useState("");
  const [message, setMessage] = useState("");

  function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (isSending) return;
    onSubmit({
      to: splitAddresses(to),
      cc: splitAddresses(cc),
      message: message.trim() || null,
    });
  }

  return (
    <form onSubmit={handleSubmit} className="flex flex-col gap-3">
      <FormField label="To" htmlFor={`${idPrefix}-to`}>
        <TextInput
          id={`${idPrefix}-to`}
          value={to}
          onChange={(e) => setTo(e.target.value)}
          placeholder={clientEmail ?? "client@example.com"}
          required={!clientEmail}
        />
        {clientEmail && (
          <p className="mt-1 text-xs text-ink-muted">
            Leave blank to send to the client email, {clientEmail}.
          </p>
        )}
      </FormField>
      <FormField label="Cc" htmlFor={`${idPrefix}-cc`}>
        <TextInput
          id={`${idPrefix}-cc`}
          value={cc}
          onChange={(e) => setCc(e.target.value)}
          placeholder="Optional, comma-separated"
        />
      </FormField>
      <FormField label="Message" htmlFor={`${idPrefix}-message`}>
        <textarea
          id={`${idPrefix}-message`}
          className={inputBase}
          rows={4}
          value={message}
          onChange={(e) => setMessage(e.target.value)}
          maxLength={5000}
          placeholder="Optional note above the summary"
        />
      </FormField>
      <p className="text-xs text-ink-muted">
        The PDF is attached. Replies go to your business email.
      </p>
      {error && (
        <p role="alert" className="text-sm text-red-600 dark:text-red-400">
          {error}
        </p>
      )}
      <div className="flex gap-2">
        <Button type="submit" disabled={isSending}>
          {isSending ? "Sending…" : submitLabel}
        </Button>
        <Button
          type="button"
          variant="secondary"
          disabled={isSending}
          onClick={onCancel}
        >
          Cancel
        </Button>
      </div>
    </form>
  );
}
