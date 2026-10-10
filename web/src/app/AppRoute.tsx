import { Suspense } from "react";
import { Navigate, Route, Routes } from "react-router";
import AuthenticatedSession from "../auth/AuthenticatedSession";
import { useCurrentUser } from "../auth/useCurrentUser";
import { lazyWithReload } from "../lib/lazyWithReload";
import LoadingIndicator from "../components/LoadingIndicator";
import AppShell from "./AppShell";
import OAuthAuthorize from "./OAuthAuthorize";
import Settings from "./Settings";
import { TicketListPage } from "./tickets/TicketListPage";
import { TicketThreadPage } from "./tickets/TicketThreadPage";
import { ProjectListPage } from "./invoicing/ProjectListPage";
import { ProjectDetailPage } from "./invoicing/ProjectDetailPage";
import { BillableItemListPage } from "./invoicing/BillableItemListPage";
import { ExpenseListPage } from "./invoicing/ExpenseListPage";
import { InvoiceListPage } from "./invoicing/InvoiceListPage";
import { InvoiceDetailPage } from "./invoicing/InvoiceDetailPage";
import { InvoicingSettingsPage } from "./invoicing/InvoicingSettingsPage";
import { useSelectedInstance } from "./SelectedInstanceContext";
import { homePathForKind } from "./selectedInstance";

// Its own lazy chunk, loaded only once someone actually navigates under
// `/app/admin/*` — most users are never superusers and never need this
// code, per the same reasoning `Router.tsx` already applies to `/app/*`
// itself, `/login`, and `/submit`.
const AdminRoute = lazyWithReload("admin", () => import("./admin/AdminRoute"));

/**
 * `/app`'s index route. A superuser with zero memberships has nothing to
 * land on at `tickets/open` — that page just shows "pick an instance"
 * forever, since a superuser gets no ticket access without a real
 * membership (see `CLAUDE.md`'s superuser boundary). Send them to the admin
 * area instead, where they actually have something to do. Anyone else
 * (including a superuser who *is* a member somewhere) lands on the selected
 * instance's home page — the ticket queue for a support instance, invoices
 * for an invoicing one. With memberships but no selection yet (the
 * switcher publishes its initial pick from an effect, one render after
 * this first mounts), it renders nothing and redirects on the next render,
 * rather than guessing a kind and bouncing.
 */
function AppIndexRedirect() {
  const user = useCurrentUser();
  const selected = useSelectedInstance();
  if (user.memberships.length === 0) {
    return (
      <Navigate to={user.isSuperuser ? "admin" : "tickets/open"} replace />
    );
  }
  if (!selected) return null;
  return <Navigate to={homePathForKind(selected.kind)} replace />;
}

/**
 * `/app/*` — the authenticated area. `AuthenticatedSession` decides between
 * the login page and this tree; everything below assumes a valid session.
 * Nested routing lives here (not in the top-level `Router`) since this
 * whole subtree is one lazy chunk already.
 */
export default function AppRoute() {
  return (
    <AuthenticatedSession>
      <Routes>
        {/* Deliberately outside AppShell: a one-off consent screen, not part of
            the app chrome — but still inside AuthenticatedSession, so a
            logged-out visit shows the login page first and comes right back to
            this same URL (query string and all) once it succeeds. */}
        <Route path="oauth/authorize" element={<OAuthAuthorize />} />
        <Route element={<AppShell />}>
          <Route index element={<AppIndexRedirect />} />
          <Route
            path="tickets/open"
            element={
              <TicketListPage
                status="OPEN"
                title="Open tickets"
                emptyMessage="No open tickets — the queue is clear."
              />
            }
          />
          <Route
            path="tickets/closed"
            element={
              <TicketListPage
                status="CLOSED"
                title="Closed tickets"
                emptyMessage="No closed tickets yet."
              />
            }
          />
          <Route
            path="tickets/all"
            element={
              <TicketListPage
                status="ALL"
                title="All tickets"
                emptyMessage="No tickets yet."
              />
            }
          />
          <Route
            path="tickets/mine"
            element={
              <TicketListPage
                status="ALL"
                assignedToMe
                title="Assigned to me"
                emptyMessage="Nothing assigned to you right now."
              />
            }
          />
          <Route
            path="tickets/deleted"
            element={
              <TicketListPage
                status="DELETED"
                title="Deleted tickets"
                emptyMessage="No deleted tickets."
              />
            }
          />
          <Route path="tickets/:id" element={<TicketThreadPage />} />
          <Route path="invoices" element={<InvoiceListPage />} />
          <Route path="invoices/:id" element={<InvoiceDetailPage />} />
          <Route path="projects" element={<ProjectListPage />} />
          <Route path="projects/:id" element={<ProjectDetailPage />} />
          <Route path="billable-items" element={<BillableItemListPage />} />
          <Route path="expenses" element={<ExpenseListPage />} />
          <Route
            path="invoicing-settings"
            element={<InvoicingSettingsPage />}
          />
          <Route path="settings" element={<Settings />} />
          <Route
            path="admin/*"
            element={
              // A local boundary, not the top-level Router one, so only the
              // content area suspends while the admin chunk loads — the
              // shell (nav, instance switcher) stays put instead of
              // momentarily disappearing.
              <Suspense fallback={<LoadingIndicator />}>
                <AdminRoute />
              </Suspense>
            }
          />
        </Route>
      </Routes>
    </AuthenticatedSession>
  );
}
