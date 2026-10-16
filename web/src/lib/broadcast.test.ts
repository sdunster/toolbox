import { describe, expect, it } from "vitest";
import {
  estimatedSendSecs,
  formatDuration,
  invalidRecipients,
  parseRecipients,
  recipientStatus,
} from "./broadcast";

describe("parseRecipients", () => {
  it("splits on newlines, commas, semicolons and spaces", () => {
    expect(
      parseRecipients(
        "a@example.com, b@example.com\nc@example.com; d@x.io e@y.io",
      ),
    ).toEqual([
      "a@example.com",
      "b@example.com",
      "c@example.com",
      "d@x.io",
      "e@y.io",
    ]);
  });

  it("lowercases and de-duplicates in first-seen order", () => {
    expect(
      parseRecipients("B@Example.com\na@example.com\nb@example.com"),
    ).toEqual(["b@example.com", "a@example.com"]);
  });

  it("ignores blank input and keeps the address from <angle brackets>", () => {
    expect(parseRecipients("  \n\n")).toEqual([]);
    expect(parseRecipients("<a@example.com>")).toEqual(["a@example.com"]);
  });
});

describe("invalidRecipients", () => {
  it("flags entries that aren't shaped like an address", () => {
    expect(invalidRecipients(["a@example.com", "nope", "@x.io", "b@"])).toEqual(
      ["nope", "@x.io", "b@"],
    );
  });
});

describe("recipientStatus", () => {
  it("prefers the delivery outcome over the send status", () => {
    expect(recipientStatus("SENT", "BOUNCED")).toEqual({
      label: "Bounced",
      tone: "bad",
    });
    expect(recipientStatus("SENT", null).label).toBe("Sent");
    expect(recipientStatus("FAILED", null).tone).toBe("bad");
    expect(recipientStatus("SUPPRESSED", null).label).toBe(
      "Skipped (suppressed)",
    );
    expect(recipientStatus("PENDING", undefined).label).toBe("Not sent");
  });

  it("reads PENDING and SENDING by whether the broadcast is still sending", () => {
    expect(recipientStatus("PENDING", null, true).label).toBe("Queued");
    expect(recipientStatus("SENDING", null, true).label).toBe("Sending…");
    expect(recipientStatus("SENDING", null, false)).toEqual({
      label: "Unknown (send interrupted)",
      tone: "warn",
    });
  });
});

describe("send duration", () => {
  it("counts the gaps between batches of 5, five minutes apart", () => {
    expect(estimatedSendSecs(0)).toBe(0);
    expect(estimatedSendSecs(5)).toBe(0);
    expect(estimatedSendSecs(6)).toBe(300);
    // 117 recipients: 24 batches, 23 gaps.
    expect(estimatedSendSecs(117)).toBe(23 * 300);
  });

  it("formats roughly", () => {
    expect(formatDuration(0)).toBe("under a minute");
    expect(formatDuration(25 * 60)).toBe("25 min");
    expect(formatDuration(60 * 60)).toBe("1 h");
    expect(formatDuration(23 * 300)).toBe("1 h 55 min");
  });
});
