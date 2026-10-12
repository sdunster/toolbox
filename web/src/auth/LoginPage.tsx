import { useEffect, useRef, useState } from "react";
import { rawGraphQL } from "../lib/rawGraphQL";
import { useTurnstile } from "../lib/turnstile";
import {
  loginWithPasskey,
  browserSupportsWebAuthn,
  browserSupportsWebAuthnAutofill,
} from "../lib/passkey";
import { Card } from "../components/ui/Card";
import { Button } from "../components/ui/Button";
import { FormField } from "../components/ui/FormField";
import TextInput from "../components/ui/TextInput";

export type EmailCodeStep = "idle" | "sending" | "awaiting_code" | "verifying";

interface LoginPageProps {
  errorMessage?: string | null;
  onNewTokenReceived: (token: string) => void;
}

/**
 * Email + 6-digit code, and passkey login. Both run via raw `fetch`
 * (`rawGraphQL`) rather than a Relay environment — there's no session token
 * yet to build one with, and building an environment just for this one
 * unauthenticated screen would be more machinery than the two mutations it
 * calls warrant.
 *
 * Rendered both as the standalone `/login` route and, via
 * `AuthenticatedSession`, in place of `/app/*` whenever there's no valid
 * session — same component either way, so the two can't drift.
 */
export default function LoginPage({
  errorMessage,
  onNewTokenReceived,
}: LoginPageProps) {
  const [step, setStep] = useState<EmailCodeStep>("idle");
  const [email, setEmail] = useState("");
  const [code, setCode] = useState("");
  const [codeError, setCodeError] = useState<string | null>(null);
  const [passkeySigningIn, setPasskeySigningIn] = useState(false);
  const [passkeyError, setPasskeyError] = useState<string | null>(null);
  const passkeySupported = browserSupportsWebAuthn();
  const {
    enabled: turnstileEnabled,
    token: currentTurnstileToken,
    failed: turnstileFailed,
    mountWidget: mountTurnstile,
    consume: consumeTurnstileToken,
  } = useTurnstile();
  // With Turnstile on, a send waits for a passed challenge; the server
  // would otherwise silently drop it (see `lib/turnstile.ts`).
  const turnstileReady = !turnstileEnabled || currentTurnstileToken !== null;

  const onNewTokenReceivedRef = useRef(onNewTokenReceived);
  useEffect(() => {
    onNewTokenReceivedRef.current = onNewTokenReceived;
  }, [onNewTokenReceived]);

  // Ensures the autofill ceremony is started only once. StrictMode (dev)
  // runs effect setup→cleanup→setup on the same instance, so without this
  // guard we'd fire beginPasskeyLogin twice and race two ceremonies.
  const autofillStartedRef = useRef(false);
  // Tracks whether the component is really mounted. StrictMode's fake
  // unmount flips this false, but its second setup flips it back true — so
  // a token from the single ceremony isn't discarded, while a real unmount
  // still discards a late-arriving result.
  const mountedRef = useRef(true);

  // Transparent passkey login: on mount, prime a discoverable challenge and
  // attach it to the email field via browser autofill (conditional UI). If
  // the user picks a saved passkey we log them straight in; otherwise this
  // is a no-op and the email-code flow remains available.
  useEffect(() => {
    mountedRef.current = true;
    if (autofillStartedRef.current) return;
    autofillStartedRef.current = true;
    (async () => {
      try {
        if (!(await browserSupportsWebAuthnAutofill())) return;
        const result = await loginWithPasskey({ useAutofill: true });
        if (!mountedRef.current) return;
        if (result.status === "ok") {
          onNewTokenReceivedRef.current(result.token);
        } else if (result.status === "failed") {
          setPasskeyError("Passkey login failed.");
        }
        // "cancelled" → stay silent (the user dismissed the autofill prompt).
      } catch (err) {
        // Conditional UI unsupported or aborted — ignore silently.
        console.warn("[passkey] autofill effect threw:", err);
      }
    })();
    return () => {
      mountedRef.current = false;
    };
  }, []);

  async function handlePasskeyLogin() {
    setPasskeySigningIn(true);
    setPasskeyError(null);
    try {
      const result = await loginWithPasskey({ useAutofill: false });
      if (result.status === "ok") {
        onNewTokenReceived(result.token);
      } else if (result.status === "failed") {
        setPasskeyError("Passkey login failed.");
      } else {
        // "cancelled" — the user dismissed the prompt or has no passkey.
        setPasskeyError(
          "No passkey was used. Make sure you've added one, or sign in another way.",
        );
      }
    } catch (err) {
      console.warn("[passkey] manual button threw:", err);
      setPasskeyError("Passkey sign-in failed. Please try again.");
    } finally {
      setPasskeySigningIn(false);
    }
  }

  // Client-side mirror of the API's own "one send per 30s per email" limit
  // (see `requestAuthCode`'s doc comment in the schema) — there's no signal
  // in the response to distinguish a rate-limited send from a real one (the
  // mutation always returns `true`, deliberately, to avoid leaking which
  // emails are registered), so the client just refuses to hammer the button
  // faster than the server would honour anyway.
  const RESEND_COOLDOWN_MS = 30_000;
  const [resendAvailableAt, setResendAvailableAt] = useState<number | null>(
    null,
  );
  const [now, setNow] = useState(() => Date.now());

  useEffect(() => {
    if (resendAvailableAt === null || now >= resendAvailableAt) return;
    const interval = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(interval);
  }, [resendAvailableAt, now]);

  const resendRemainingMs =
    resendAvailableAt !== null ? Math.max(0, resendAvailableAt - now) : 0;
  const canResend =
    resendAvailableAt !== null && resendRemainingMs <= 0 && turnstileReady;

  async function sendCode() {
    setStep("sending");
    setCodeError(null);
    // `null` when Turnstile is off for this build — which the API accepts
    // only while it has no `TURNSTILE_SECRET_KEY` either.
    const turnstileToken = consumeTurnstileToken();
    try {
      await rawGraphQL(
        `mutation RequestAuthCode($email: String!, $turnstileToken: String) {
          requestAuthCode(email: $email, turnstileToken: $turnstileToken)
        }`,
        { email, turnstileToken },
      );
      const sentAt = Date.now();
      setNow(sentAt);
      setResendAvailableAt(sentAt + RESEND_COOLDOWN_MS);
      setStep("awaiting_code");
      setCode("");
    } catch {
      setCodeError("Failed to send code. Please try again.");
      setStep(resendAvailableAt === null ? "idle" : "awaiting_code");
    }
  }

  function handleSendCode(e: React.FormEvent) {
    e.preventDefault();
    void sendCode();
  }

  function handleResendCode() {
    if (!canResend) return;
    void sendCode();
  }

  async function handleVerifyCode(e: React.FormEvent) {
    e.preventDefault();
    setStep("verifying");
    setCodeError(null);
    try {
      const result = await rawGraphQL<{ verifyAuthCode: string | null }>(
        `mutation VerifyAuthCode($email: String!, $code: String!) {
          verifyAuthCode(email: $email, code: $code)
        }`,
        { email, code },
      );

      const token = result.data?.verifyAuthCode;
      if (token) {
        onNewTokenReceived(token);
      } else {
        setCodeError("Incorrect or expired code. Please try again.");
        setStep("awaiting_code");
        setCode("");
      }
    } catch {
      setCodeError("Verification failed. Please try again.");
      setStep("awaiting_code");
    }
  }

  function startOver() {
    setStep("idle");
    setCode("");
    setCodeError(null);
    setResendAvailableAt(null);
  }

  return (
    <div className="flex min-h-screen items-center justify-center bg-surface px-4 py-12">
      <Card className="w-full max-w-md">
        <h1 className="mb-1 text-2xl font-semibold text-ink-strong">Log in</h1>
        <p className="mb-6 text-sm text-ink-muted">
          Sign in to work your team&apos;s ticket queue.
        </p>

        {errorMessage ? (
          <p
            role="alert"
            className="mb-4 text-sm text-red-600 dark:text-red-400"
          >
            {errorMessage}
          </p>
        ) : null}

        {passkeySupported && (
          <div className="mb-4">
            <Button
              className="w-full"
              variant="secondary"
              onClick={handlePasskeyLogin}
              disabled={passkeySigningIn}
            >
              {passkeySigningIn
                ? "Waiting for passkey…"
                : "Sign in with a passkey"}
            </Button>
            {passkeyError && (
              <p
                role="alert"
                className="mt-2 text-sm text-red-600 dark:text-red-400"
              >
                {passkeyError}
              </p>
            )}
            <div className="my-4 flex items-center gap-3 text-xs text-ink-muted">
              <div className="h-px flex-1 bg-line" />
              or
              <div className="h-px flex-1 bg-line" />
            </div>
          </div>
        )}

        {step === "idle" && (
          <form onSubmit={handleSendCode} className="flex flex-col gap-4">
            <FormField label="Email address" htmlFor="login-email">
              <TextInput
                id="login-email"
                type="email"
                placeholder="you@example.com"
                autoComplete="username webauthn"
                value={email}
                onChange={(e) => setEmail(e.target.value)}
                required
              />
            </FormField>
            {codeError && (
              <p
                role="alert"
                className="text-sm text-red-600 dark:text-red-400"
              >
                {codeError}
              </p>
            )}
            <Button type="submit" disabled={!email || !turnstileReady}>
              Send code
            </Button>
          </form>
        )}

        {step === "sending" && (
          <p className="text-sm text-ink-muted">Sending code to {email}…</p>
        )}

        {step === "awaiting_code" && (
          <form onSubmit={handleVerifyCode} className="flex flex-col gap-4">
            <p className="text-sm text-ink-muted">
              If <strong>{email}</strong> has an account, a 6-digit code has
              been sent. Enter it below to log in.
            </p>
            {codeError && (
              <p
                role="alert"
                className="text-sm text-red-600 dark:text-red-400"
              >
                {codeError}
              </p>
            )}
            <FormField label="6-digit code" htmlFor="login-code">
              <TextInput
                id="login-code"
                type="text"
                inputMode="numeric"
                pattern="[0-9]{6}"
                maxLength={6}
                autoComplete="one-time-code"
                placeholder="123456"
                value={code}
                onChange={(e) => setCode(e.target.value)}
                required
                autoFocus
              />
            </FormField>
            <div className="flex flex-wrap gap-2">
              <Button type="submit" disabled={code.length !== 6}>
                Verify code
              </Button>
              <Button type="button" variant="secondary" onClick={startOver}>
                Start over
              </Button>
            </div>
            <Button
              type="button"
              variant="ghost"
              className="self-start px-0"
              onClick={handleResendCode}
              disabled={!canResend}
            >
              {canResend
                ? "Resend code"
                : `Resend code in ${Math.ceil(resendRemainingMs / 1000)}s`}
            </Button>
          </form>
        )}

        {step === "verifying" && (
          <p className="text-sm text-ink-muted">Verifying…</p>
        )}

        {/* Mounted across every step so a resend has a fresh token ready. */}
        {turnstileEnabled && <div ref={mountTurnstile} className="mt-4" />}
        {turnstileFailed && (
          <p
            role="alert"
            className="mt-2 text-sm text-red-600 dark:text-red-400"
          >
            Couldn&apos;t confirm you&apos;re not a bot. Reload the page and try
            again.
          </p>
        )}
      </Card>
    </div>
  );
}
