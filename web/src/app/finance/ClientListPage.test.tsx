import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import UserEvent from "@testing-library/user-event";
import { setupServer } from "msw/node";
import { graphql as mswGraphql, HttpResponse } from "msw";
import { RelayEnvironmentProvider } from "react-relay";
import { MemoryRouter } from "react-router";
import { getGraphQLEndpoint } from "../../lib/api";
import { createUnauthenticatedGraphQLEnvironment } from "../../lib/environments";
import { SelectedInstanceContext } from "../SelectedInstanceContext";
import type { SelectedInstance } from "../selectedInstance";
import { ClientListPage } from "./ClientListPage";

const relayEndpoint = mswGraphql.link(getGraphQLEndpoint());
const server = setupServer();

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

const instance: SelectedInstance = {
  id: "inst-ledger",
  slug: "ledger",
  name: "Ledger & Co Bookkeeping",
  role: "AGENT",
  kind: "FINANCE",
};

const retail = {
  __typename: "Client",
  id: "client-1",
  name: "Fictional Retail Pty Ltd",
  abn: "55 666 777 888",
  address: "22 Example Street",
  email: "accounts@retail.example.com",
  paymentTermsDays: 30,
  effectivePaymentTermsDays: 30,
  archived: false,
  projects: [{ __typename: "Project", id: "proj-1", name: "Website Redesign" }],
};

function renderPage() {
  const environment = createUnauthenticatedGraphQLEnvironment();
  return render(
    <MemoryRouter initialEntries={["/app/clients"]}>
      <RelayEnvironmentProvider environment={environment}>
        <SelectedInstanceContext value={instance}>
          <ClientListPage />
        </SelectedInstanceContext>
      </RelayEnvironmentProvider>
    </MemoryRouter>,
  );
}

describe("ClientListPage", () => {
  it("lists clients with their terms and projects", async () => {
    server.use(
      relayEndpoint.query("ClientListPageQuery", () =>
        HttpResponse.json({ data: { clients: [retail] } }),
      ),
    );
    renderPage();
    expect(
      await screen.findByText("Fictional Retail Pty Ltd"),
    ).toBeInTheDocument();
    expect(
      screen.getByText("accounts@retail.example.com · 30-day terms"),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("link", { name: "Website Redesign" }),
    ).toHaveAttribute("href", "/app/projects/proj-1");
  });

  it("creates a client, sending blank terms as null", async () => {
    let received: Record<string, unknown> | undefined;
    server.use(
      relayEndpoint.query("ClientListPageQuery", () =>
        HttpResponse.json({ data: { clients: [] } }),
      ),
      relayEndpoint.mutation(
        "ClientListPageCreateMutation",
        ({ variables }) => {
          received = variables;
          return HttpResponse.json({
            data: { createClient: { __typename: "Client", id: "client-2" } },
          });
        },
      ),
    );
    const user = UserEvent.setup();
    renderPage();
    await screen.findByText("No clients yet — add one below.");
    await user.type(screen.getByLabelText("Client name"), " Zed Holdings ");
    await user.type(screen.getByLabelText("Email"), "ap@zed.example.com");
    await user.click(screen.getByRole("button", { name: "Create client" }));
    await waitFor(() =>
      expect(received).toEqual({
        instanceId: "inst-ledger",
        input: {
          name: "Zed Holdings",
          abn: "",
          address: "",
          email: "ap@zed.example.com",
          paymentTermsDays: null,
        },
      }),
    );
  });

  it("edits and archives a client", async () => {
    let received: Record<string, unknown> | undefined;
    server.use(
      relayEndpoint.query("ClientListPageQuery", () =>
        HttpResponse.json({ data: { clients: [retail] } }),
      ),
      relayEndpoint.mutation(
        "ClientListPageUpdateMutation",
        ({ variables }) => {
          received = variables;
          return HttpResponse.json({
            data: { updateClient: { ...retail, archived: true } },
          });
        },
      ),
    );
    const user = UserEvent.setup();
    renderPage();
    await user.click(await screen.findByRole("button", { name: "Edit" }));
    // The edit form comes before the "New client" form.
    await user.clear(screen.getAllByLabelText("Payment terms (days)")[0]);
    await user.click(
      screen.getByLabelText("Archived (hidden from new projects)"),
    );
    await user.click(screen.getByRole("button", { name: "Save" }));
    await waitFor(() =>
      expect(received).toEqual({
        id: "client-1",
        input: {
          name: "Fictional Retail Pty Ltd",
          abn: "55 666 777 888",
          address: "22 Example Street",
          email: "accounts@retail.example.com",
          paymentTermsDays: null,
        },
        archived: true,
      }),
    );
    expect(await screen.findByText("Archived")).toBeInTheDocument();
  });
});
