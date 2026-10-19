import { FormField } from "../../components/ui/FormField";
import TextInput from "../../components/ui/TextInput";
import { inputBase } from "../../components/ui/inputStyles";
import { formatSender, type BroadcastSender } from "../../lib/broadcast";
import {
  CUSTOM_SENDER,
  resolveSender,
  type SenderChoice,
} from "./senderChoice";

/**
 * "Send as" (with saved senders) or the From address and name fields —
 * shared by the plain-text form and the HTML editor.
 */
export function SenderFields({
  id,
  choice,
  onChange,
  senders,
  domains,
  defaultFromName,
}: {
  id: string;
  choice: SenderChoice;
  onChange: (choice: SenderChoice) => void;
  senders: readonly BroadcastSender[];
  domains: readonly string[];
  defaultFromName: string;
}) {
  const { sender } = resolveSender(choice, senders);
  return (
    <>
      {senders.length > 0 && (
        <FormField label="Send as" htmlFor={`${id}-sender`}>
          <select
            id={`${id}-sender`}
            className={inputBase}
            value={sender ? choice.senderId : CUSTOM_SENDER}
            onChange={(e) => onChange({ ...choice, senderId: e.target.value })}
          >
            {senders.map((s) => (
              <option key={s.id} value={s.id}>
                {formatSender(s)}
              </option>
            ))}
            <option value={CUSTOM_SENDER}>Another address…</option>
          </select>
        </FormField>
      )}

      {!sender && (
        <div className="grid gap-4 sm:grid-cols-2">
          <FormField label="From address" htmlFor={`${id}-from`}>
            <div className="flex items-center gap-1">
              <TextInput
                id={`${id}-from`}
                placeholder="news"
                value={choice.localPart}
                onChange={(e) =>
                  onChange({ ...choice, localPart: e.target.value })
                }
              />
              <span className="text-sm text-ink-muted">@</span>
              {domains.length === 1 ? (
                <span className="text-sm text-ink">{choice.domain}</span>
              ) : (
                <select
                  aria-label="From domain"
                  className={inputBase}
                  value={choice.domain}
                  onChange={(e) =>
                    onChange({ ...choice, domain: e.target.value })
                  }
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
              value={choice.fromName}
              onChange={(e) =>
                onChange({ ...choice, fromName: e.target.value })
              }
            />
          </FormField>
        </div>
      )}
    </>
  );
}
