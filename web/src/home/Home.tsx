import { Navigate } from "react-router";
import { ButtonLink } from "../components/ui/Button";
import { Card } from "../components/ui/Card";
import { getSessionToken } from "../lib/sessionToken";

/**
 * The front door: `/`. Two ways in, matching the build plan — log in (staff
 * working the queue) or submit a new ticket (a member of the public). No
 * data fetching, no auth — this page exists to route the two audiences
 * apart before anything else loads.
 *
 * Someone who already holds a session token skips straight to `/app`
 * rather than being asked to "Log in" again. The token isn't validated
 * here: if it has expired, `/app`'s `AuthenticatedSession` gets the 401 and
 * shows the login page in place, same as any other stale session.
 */
export default function Home() {
  if (getSessionToken()) {
    return <Navigate to="/app" replace />;
  }

  return (
    <main className="flex min-h-screen flex-col items-center justify-center gap-10 bg-surface px-4 py-16">
      <div className="text-center">
        <h1 className="text-4xl font-semibold text-ink-strong">Toolbox</h1>
        <p className="mt-3 max-w-md text-ink-muted">
          A small shared inbox for support requests — email in, a queue your
          team can work from the web.
        </p>
      </div>

      <div className="grid w-full max-w-2xl grid-cols-1 gap-6 sm:grid-cols-2">
        <Card className="flex flex-col items-start gap-3">
          <h2 className="text-lg font-semibold text-ink-strong">I work here</h2>
          <p className="text-sm text-ink-muted">
            Log in to view and reply to your team&apos;s tickets.
          </p>
          <ButtonLink to="/login" size="large" className="mt-auto">
            Log in
          </ButtonLink>
        </Card>

        <Card className="flex flex-col items-start gap-3">
          <h2 className="text-lg font-semibold text-ink-strong">I need help</h2>
          <p className="text-sm text-ink-muted">
            Raise a new support ticket — no account required.
          </p>
          <ButtonLink
            to="/submit"
            variant="secondary"
            size="large"
            className="mt-auto"
          >
            Submit a new ticket
          </ButtonLink>
        </Card>
      </div>
    </main>
  );
}
