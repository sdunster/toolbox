import { Suspense } from "react";
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { setupServer } from "msw/node";
import { graphql as mswGraphql, HttpResponse } from "msw";
import { RelayEnvironmentProvider } from "react-relay";
import { MemoryRouter, Route, Routes } from "react-router";
import { getGraphQLEndpoint } from "../lib/api";
import { createUnauthenticatedGraphQLEnvironment } from "../lib/environments";
import { CurrentUserProvider } from "../auth/CurrentUserProvider";
import { LogoutContext } from "../auth/useLogout";
import AppShell from "./AppShell";

const relayEndpoint = mswGraphql.link(getGraphQLEndpoint());
const server = setupServer();

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => {
  server.resetHandlers();
  localStorage.clear();
});
afterAll(() => server.close());

function membership(role: "OWNER" | "AGENT", kind: "SUPPORT" | "INVOICING") {
  const slug = kind === "INVOICING" ? "ledger" : "acme";
  return {
    role,
    instance: { id: `inst-${slug}`, name: slug, slug, kind },
  };
}

function meResponse(
  isSuperuser: boolean,
  memberships: ReturnType<typeof membership>[],
) {
  return {
    data: {
      me: {
        id: "user-1",
        email: "agent@example.com",
        name: "Adrian Agent",
        enabled: true,
        isSuperuser,
        memberships,
      },
    },
  };
}

function renderShell(
  isSuperuser: boolean,
  memberships: ReturnType<typeof membership>[] = [],
) {
  server.use(
    relayEndpoint.query("CurrentUserProviderQuery", () =>
      HttpResponse.json(meResponse(isSuperuser, memberships)),
    ),
  );
  const environment = createUnauthenticatedGraphQLEnvironment();
  return render(
    <MemoryRouter initialEntries={["/app"]}>
      <RelayEnvironmentProvider environment={environment}>
        <Suspense fallback="loading">
          <CurrentUserProvider>
            <LogoutContext value={() => {}}>
              <Routes>
                <Route path="/app" element={<AppShell />} />
              </Routes>
            </LogoutContext>
          </CurrentUserProvider>
        </Suspense>
      </RelayEnvironmentProvider>
    </MemoryRouter>,
  );
}

describe("AppShell — Admin nav item", () => {
  it("is hidden for a non-superuser", async () => {
    renderShell(false);
    await screen.findByText("agent@example.com");
    expect(
      screen.queryByRole("link", { name: "Admin" }),
    ).not.toBeInTheDocument();
  });

  it("is shown for a superuser", async () => {
    renderShell(true);
    await screen.findByText("agent@example.com");
    expect(screen.getByRole("link", { name: "Admin" })).toBeInTheDocument();
  });
});

function navLinkNames() {
  return screen
    .getAllByRole("link")
    .map((link) => link.textContent)
    .filter(Boolean);
}

describe("AppShell — kind-aware nav", () => {
  it("shows the ticket queues for a support instance", async () => {
    renderShell(false, [membership("OWNER", "SUPPORT")]);
    // Owner-only, so it appears once the switcher publishes its selection.
    await screen.findByRole("link", { name: "Deleted" });
    expect(navLinkNames()).toEqual([
      "Open",
      "Closed",
      "All",
      "Mine",
      "Deleted",
      "Settings",
    ]);
  });

  it("shows invoices, projects and business settings to an invoicing owner", async () => {
    renderShell(true, [membership("OWNER", "INVOICING")]);
    await screen.findByRole("link", { name: "Projects" });
    expect(navLinkNames()).toEqual([
      "Invoices",
      "Projects",
      "Billable items",
      "Expenses",
      "Reports",
      "Settings",
      "Business settings",
      "Admin",
    ]);
  });

  it("hides business settings from an invoicing agent", async () => {
    renderShell(false, [membership("AGENT", "INVOICING")]);
    await screen.findByRole("link", { name: "Projects" });
    expect(navLinkNames()).toEqual([
      "Invoices",
      "Projects",
      "Billable items",
      "Expenses",
      "Reports",
      "Settings",
    ]);
  });
});
