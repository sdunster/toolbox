import { useEffect, useState } from "react";
import { rawGraphQL } from "../lib/rawGraphQL";
import { useTurnstile } from "../lib/turnstile";
import { Card } from "../components/ui/Card";
import { Button } from "../components/ui/Button";
import { ButtonLink } from "../components/ui/Button";
import { FormField } from "../components/ui/FormField";
import TextInput from "../components/ui/TextInput";
import { SubmitTicketForm } from "./SubmitTicketForm";

type Step =
  | { kind: "email" }
  | { kind: "sending_code" }
  | { kind: "awaiting_code" }
  | { kind: "verifying_code" }
  | { kind: "ticket_form"; token: string }
  | { kind: "done"; ticketNumber: number };

const RESEND_COOLDOWN_MS = 30_000;

/**
 * `/submit/:slug`: email → six-digit code → subject/body. **No passkey
 * option**, per the build plan — this is an anonymous requester, not a
 * member logging in. The email/code steps run over `rawGraphQL` (the same
 * pre-session seam `auth/LoginPage` uses) since there is no token, and thus
 * no Relay environment, until `verifySubmitCode` succeeds; the final step
 * hands off to `SubmitTicketForm`, which builds the requester Relay
 * environment from that token.
 */
export function SubmitForm({
  slug,
  instanceName,
}: {
  slug: string;
  instanceName?: string;
}) {
  const [step, setStep] = useState<Step>({ kind: "email" });
  const [email, setEmail] = useState("");
  const [code, setCode] = useState("");
  const [error, setError] = useState<string | null>(null);
  const [resendAvailableAt, setResendAvailableAt] = useState<number | null>(
    null,
  );
  const [now, setNow] = useState(() => Date.now());
  const {
    enabled: turnstileEnabled,
    token: currentTurnstileToken,
    failed: turnstileFailed,
    mountWidget: mountTurnstile,
    consume: consumeTurnstileToken,
  } = useTurnstile();
  // Same gate as `auth/LoginPage`: with Turnstile on, a send waits for a
  // passed challenge.
  const turnstileReady = !turnstileEnabled || currentTurnstileToken !== null;

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
    setStep({ kind: "sending_code" });
    setError(null);
    const turnstileToken = consumeTurnstileToken();
    try {
      await rawGraphQL(
        `mutation RequestSubmitCode($slug: String!, $email: String!, $turnstileToken: String) {
          requestSubmitCode(slug: $slug, email: $email, turnstileToken: $turnstileToken)
        }`,
        { slug, email, turnstileToken },
      );
      const sentAt = Date.now();
      setNow(sentAt);
      setResendAvailableAt(sentAt + RESEND_COOLDOWN_MS);
      setCode("");
      setStep({ kind: "awaiting_code" });
    } catch {
      setError("Failed to send code. Please try again.");
      setStep({ kind: "email" });
    }
  }

  function handleSendCode(e: React.FormEvent) {
    e.preventDefault();
    void sendCode();
  }

  function handleResend() {
    if (!canResend) return;
    void sendCode();
  }

  async function handleVerifyCode(e: React.FormEvent) {
    e.preventDefault();
    setStep({ kind: "verifying_code" });
    setError(null);
    try {
      const result = await rawGraphQL<{ verifySubmitCode: string | null }>(
        `mutation VerifySubmitCode($slug: String!, $email: String!, $code: String!) {
          verifySubmitCode(slug: $slug, email: $email, code: $code)
        }`,
        { slug, email, code },
      );
      const token = result.data?.verifySubmitCode;
      if (token) {
        setStep({ kind: "ticket_form", token });
      } else {
        setError("Incorrect or expired code. Please try again.");
        setCode("");
        setStep({ kind: "awaiting_code" });
      }
    } catch {
      setError("Verification failed. Please try again.");
      setStep({ kind: "awaiting_code" });
    }
  }

  if (step.kind === "done") {
    return (
      <Card className="w-full max-w-md text-center">
        <h1 className="text-xl font-semibold text-ink-strong">
          Ticket #{step.ticketNumber} submitted
        </h1>
        <p className="mt-2 text-sm text-ink-muted">
          We&apos;ve emailed {email} a confirmation. Reply to that email any
          time to add more information — there&apos;s no need to come back here.
        </p>
        <ButtonLink to="/" variant="secondary" className="mt-4">
          Back to start
        </ButtonLink>
      </Card>
    );
  }

  return (
    <Card className="w-full max-w-md">
      <h1 className="text-xl font-semibold text-ink-strong">
        {instanceName ? `Contact ${instanceName}` : "Submit a ticket"}
      </h1>
      <p className="mb-6 text-sm text-ink-muted">
        We&apos;ll verify your email with a code, then you can describe what you
        need help with.
      </p>

      {error && (
        <p role="alert" className="mb-4 text-sm text-red-600 dark:text-red-400">
          {error}
        </p>
      )}

      {(step.kind === "email" || step.kind === "sending_code") && (
        <form onSubmit={handleSendCode} className="flex flex-col gap-4">
          <FormField label="Email address" htmlFor="submit-email">
            <TextInput
              id="submit-email"
              type="email"
              placeholder="you@example.com"
              value={email}
              onChange={(e) => setEmail(e.target.value)}
              required
            />
          </FormField>
          <Button
            type="submit"
            disabled={!email || step.kind === "sending_code" || !turnstileReady}
          >
            {step.kind === "sending_code" ? "Sending…" : "Send code"}
          </Button>
        </form>
      )}

      {(step.kind === "awaiting_code" || step.kind === "verifying_code") && (
        <form onSubmit={handleVerifyCode} className="flex flex-col gap-4">
          <p className="text-sm text-ink-muted">
            If <strong>{email}</strong> can receive mail, a 6-digit code has
            been sent. Enter it below.
          </p>
          <FormField label="6-digit code" htmlFor="submit-code">
            <TextInput
              id="submit-code"
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
            <Button
              type="submit"
              disabled={code.length !== 6 || step.kind === "verifying_code"}
            >
              {step.kind === "verifying_code" ? "Verifying…" : "Verify code"}
            </Button>
            <Button
              type="button"
              variant="secondary"
              onClick={() => setStep({ kind: "email" })}
            >
              Start over
            </Button>
          </div>
          <Button
            type="button"
            variant="ghost"
            className="self-start px-0"
            onClick={handleResend}
            disabled={!canResend}
          >
            {canResend
              ? "Resend code"
              : `Resend code in ${Math.ceil(resendRemainingMs / 1000)}s`}
          </Button>
        </form>
      )}

      {/* Mounted across the email/code steps so a resend has a fresh
          token ready; gone once the code is verified. */}
      {turnstileEnabled && step.kind !== "ticket_form" && (
        <div ref={mountTurnstile} className="mt-4" />
      )}
      {turnstileFailed && step.kind !== "ticket_form" && (
        <p role="alert" className="mt-2 text-sm text-red-600 dark:text-red-400">
          Couldn&apos;t confirm you&apos;re not a bot. Reload the page and try
          again.
        </p>
      )}

      {step.kind === "ticket_form" && (
        <SubmitTicketForm
          token={step.token}
          onSubmitted={(ticketNumber) =>
            setStep({ kind: "done", ticketNumber })
          }
        />
      )}
    </Card>
  );
}
