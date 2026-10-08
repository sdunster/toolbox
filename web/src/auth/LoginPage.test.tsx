import {
  afterAll,
  afterEach,
  beforeAll,
  beforeEach,
  describe,
  expect,
  it,
  vi,
} from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import UserEvent from "@testing-library/user-event";
import { setupServer } from "msw/node";
import { graphql, HttpResponse } from "msw";
import { getGraphQLEndpoint } from "../lib/api";
import LoginPage from "./LoginPage";

// Mocked module-wide: jsdom has no WebAuthn implementation, so the real
// `@simplewebauthn/browser` would already report "unsupported" — but the
// "dismissed passkey prompt" test needs to simulate a *supported* browser
// where the user declines, which jsdom can't do on its own.
vi.mock("@simplewebauthn/browser", () => ({
  startAuthentication: vi.fn(),
  startRegistration: vi.fn(),
  browserSupportsWebAuthn: vi.fn(() => false),
  browserSupportsWebAuthnAutofill: vi.fn(async () => false),
}));

const relayEndpoint = graphql.link(getGraphQLEndpoint());
const server = setupServer();

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
beforeEach(async () => {
  const webauthn = await import("@simplewebauthn/browser");
  vi.mocked(webauthn.browserSupportsWebAuthn).mockReturnValue(false);
  vi.mocked(webauthn.browserSupportsWebAuthnAutofill).mockResolvedValue(false);
});
afterEach(() => {
  server.resetHandlers();
  vi.clearAllMocks();
});
afterAll(() => server.close());

describe("LoginPage — email code flow", () => {
  it("sends a code, then reports a wrong code and lets the user retry", async () => {
    const requestCodeCalls: unknown[] = [];
    server.use(
      relayEndpoint.mutation("RequestAuthCode", ({ variables }) => {
        requestCodeCalls.push(variables);
        return HttpResponse.json({ data: { requestAuthCode: true } });
      }),
      relayEndpoint.mutation("VerifyAuthCode", () =>
        HttpResponse.json({ data: { verifyAuthCode: null } }),
      ),
    );

    const user = UserEvent.setup();
    const onNewTokenReceived = vi.fn();
    render(<LoginPage onNewTokenReceived={onNewTokenReceived} />);

    await user.type(
      screen.getByLabelText("Email address"),
      "owner@example.com",
    );
    await user.click(screen.getByRole("button", { name: "Send code" }));

    await waitFor(() => expect(requestCodeCalls).toHaveLength(1));
    expect(requestCodeCalls[0]).toMatchObject({
      email: "owner@example.com",
      turnstileToken: null,
    });

    const codeInput = await screen.findByLabelText("6-digit code");
    await user.type(codeInput, "000000");
    await user.click(screen.getByRole("button", { name: "Verify code" }));

    expect(
      await screen.findByText("Incorrect or expired code. Please try again."),
    ).toBeInTheDocument();
    // Back on the code screen, ready to retry, with the wrong code cleared.
    expect(screen.getByLabelText("6-digit code")).toHaveValue("");
    expect(onNewTokenReceived).not.toHaveBeenCalled();
  });

  it("logs in on a correct code", async () => {
    server.use(
      relayEndpoint.mutation("RequestAuthCode", () =>
        HttpResponse.json({ data: { requestAuthCode: true } }),
      ),
      relayEndpoint.mutation("VerifyAuthCode", () =>
        HttpResponse.json({ data: { verifyAuthCode: "mtu_abc123.secret" } }),
      ),
    );

    const user = UserEvent.setup();
    const onNewTokenReceived = vi.fn();
    render(<LoginPage onNewTokenReceived={onNewTokenReceived} />);

    await user.type(
      screen.getByLabelText("Email address"),
      "owner@example.com",
    );
    await user.click(screen.getByRole("button", { name: "Send code" }));

    const codeInput = await screen.findByLabelText("6-digit code");
    await user.type(codeInput, "123456");
    await user.click(screen.getByRole("button", { name: "Verify code" }));

    await waitFor(() =>
      expect(onNewTokenReceived).toHaveBeenCalledWith("mtu_abc123.secret"),
    );
  });
});

describe("LoginPage — rate-limited resend", () => {
  // Only `Date` is faked — `setTimeout`/`setInterval` (and the real `fetch`
  // msw intercepts) stay on the real clock, so nothing here needs to drive a
  // fake event loop through an async network round trip.
  beforeEach(() => {
    vi.useFakeTimers({ toFake: ["Date"] });
  });
  afterEach(() => {
    vi.useRealTimers();
  });

  it("disables resend until the cooldown elapses, then allows it", async () => {
    let requestCodeCount = 0;
    server.use(
      relayEndpoint.mutation("RequestAuthCode", () => {
        requestCodeCount += 1;
        return HttpResponse.json({ data: { requestAuthCode: true } });
      }),
    );

    const user = UserEvent.setup({ delay: null });
    render(<LoginPage onNewTokenReceived={vi.fn()} />);

    await user.type(
      screen.getByLabelText("Email address"),
      "owner@example.com",
    );
    await user.click(screen.getByRole("button", { name: "Send code" }));

    await screen.findByLabelText("6-digit code");
    await waitFor(() => expect(requestCodeCount).toBe(1));

    const resendButton = screen.getByRole("button", {
      name: /Resend code in \d+s/,
    });
    expect(resendButton).toBeDisabled();

    await user.click(resendButton);
    // Still within the cooldown — clicking a disabled button is a no-op.
    expect(requestCodeCount).toBe(1);

    // Jump the clock forward past the 30s cooldown. Only `Date` is faked
    // (not timers): the component's own real `setInterval` still ticks on
    // the real clock, and on its next real tick reads this moved-forward
    // `Date.now()` and notices the cooldown has elapsed — which is what
    // flips the button, without a real fetch or msw handler ever having to
    // run under faked timers.
    vi.setSystemTime(Date.now() + 30_000);

    const enabledResend = await screen.findByRole(
      "button",
      { name: "Resend code" },
      { timeout: 3000 },
    );
    expect(enabledResend).toBeEnabled();

    await user.click(enabledResend);
    await waitFor(() => expect(requestCodeCount).toBe(2));
  });
});

describe("LoginPage — passkey", () => {
  it("stays silent when the autofill passkey prompt is dismissed", async () => {
    const webauthn = await import("@simplewebauthn/browser");
    vi.mocked(webauthn.browserSupportsWebAuthn).mockReturnValue(true);
    vi.mocked(webauthn.browserSupportsWebAuthnAutofill).mockResolvedValue(true);
    vi.mocked(webauthn.startAuthentication).mockRejectedValue(
      new Error("The user dismissed the prompt"),
    );

    server.use(
      relayEndpoint.mutation("BeginPasskeyLogin", () =>
        HttpResponse.json({
          data: {
            beginPasskeyLogin: {
              challengeId: "chal-1",
              optionsJson: "{}",
            },
          },
        }),
      ),
    );

    const onNewTokenReceived = vi.fn();
    render(<LoginPage onNewTokenReceived={onNewTokenReceived} />);

    // The autofill ceremony runs on mount and is dismissed
    // (startAuthentication rejects); the normal login form must still be
    // there, with no error shown anywhere on the page.
    await waitFor(() =>
      expect(webauthn.startAuthentication).toHaveBeenCalled(),
    );
    expect(screen.getByLabelText("Email address")).toBeInTheDocument();
    expect(screen.queryByRole("alert")).not.toBeInTheDocument();
    expect(onNewTokenReceived).not.toHaveBeenCalled();
  });
});
