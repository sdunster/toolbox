import type { BroadcastSender } from "../../lib/broadcast";

/** The "Send as" option for typing an address instead of a saved sender. */
export const CUSTOM_SENDER = "";

/** What the sender fields hold: a saved sender, or a typed address. */
export interface SenderChoice {
  senderId: string;
  localPart: string;
  domain: string;
  fromName: string;
}

/** The initial choice: the first saved sender, else an empty typed address. */
export function initialSenderChoice(
  senders: readonly BroadcastSender[],
  domains: readonly string[],
): SenderChoice {
  return {
    senderId: senders[0]?.id ?? CUSTOM_SENDER,
    localPart: "",
    domain: domains[0] ?? "",
    fromName: "",
  };
}

/**
 * The choice that reproduces a saved From address and name — the matching
 * saved sender if there is one, else the address typed in.
 */
export function senderChoiceFor(
  fromAddress: string,
  fromName: string,
  senders: readonly BroadcastSender[],
  domains: readonly string[],
): SenderChoice {
  const saved = senders.find(
    (s) => s.address === fromAddress && s.name === fromName,
  );
  if (saved)
    return { ...initialSenderChoice(senders, domains), senderId: saved.id };
  const [localPart, domain] = fromAddress.split("@");
  return {
    senderId: CUSTOM_SENDER,
    localPart: localPart ?? "",
    domain: domain ?? domains[0] ?? "",
    fromName,
  };
}

/**
 * The From address and name a choice resolves to. A sender removed since it
 * was picked falls back to the typed address.
 */
export function resolveSender(
  choice: SenderChoice,
  senders: readonly BroadcastSender[],
): {
  sender: BroadcastSender | undefined;
  fromAddress: string;
  fromName: string;
  complete: boolean;
} {
  const sender = senders.find((s) => s.id === choice.senderId);
  if (sender) {
    return {
      sender,
      fromAddress: sender.address,
      fromName: sender.name,
      complete: true,
    };
  }
  return {
    sender: undefined,
    fromAddress: `${choice.localPart.trim().toLowerCase()}@${choice.domain}`,
    fromName: choice.fromName.trim(),
    complete: choice.localPart.trim() !== "" && choice.domain !== "",
  };
}
