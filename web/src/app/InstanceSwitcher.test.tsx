import { Suspense, useState } from "react";
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import UserEvent from "@testing-library/user-event";
import { setupServer } from "msw/node";
import { graphql as mswGraphql, HttpResponse } from "msw";
import {
  graphql,
  useLazyLoadQuery,
  RelayEnvironmentProvider,
} from "react-relay";
import { MemoryRouter, useLocation } from "react-router";
import { getGraphQLEndpoint } from "../lib/api";
import { createUnauthenticatedGraphQLEnvironment } from "../lib/environments";
import { InstanceSwitcher } from "./InstanceSwitcher";
import type { SelectedInstance } from "./selectedInstance";
import type { InstanceSwitcherTestQuery } from "./__generated__/InstanceSwitcherTestQuery.graphql";

const relayEndpoint = mswGraphql.link(getGraphQLEndpoint());
const server = setupServer();

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => {
  server.resetHandlers();
  localStorage.clear();
});
afterAll(() => server.close());

const harnessQuery = graphql`
  query InstanceSwitcherTestQuery @throwOnFieldError {
    me {
      ...InstanceSwitcher_user
    }
  }
`;

const SUPPORT_MEMBERSHIPS = [
  {
    role: "OWNER",
    instance: {
      id: "inst-acme",
      name: "Acme Support",
      slug: "acme",
      kind: "SUPPORT",
    },
  },
  {
    role: "AGENT",
    instance: {
      id: "inst-ridge",
      name: "Ridgeline Help Desk",
      slug: "ridgeline",
      kind: "SUPPORT",
    },
  },
];

const FINANCE_MEMBERSHIP = {
  role: "OWNER",
  instance: {
    id: "inst-ledger",
    name: "Ledger Finance",
    slug: "ledger",
    kind: "FINANCE",
  },
};

function meResponse(memberships: unknown[] = SUPPORT_MEMBERSHIPS) {
  return { data: { me: { id: "user-1", memberships } } };
}

function mockMe(memberships?: unknown[]) {
  server.use(
    relayEndpoint.query("InstanceSwitcherTestQuery", () =>
      HttpResponse.json(meResponse(memberships)),
    ),
  );
}

function LocationProbe() {
  return <p data-testid="location">{useLocation().pathname}</p>;
}

function Harness() {
  const data = useLazyLoadQuery<InstanceSwitcherTestQuery>(harnessQuery, {});
  const [selected, setSelected] = useState<SelectedInstance | null>(null);
  return (
    <>
      <InstanceSwitcher
        user={data.me}
        selected={selected}
        onChange={setSelected}
      />
      <p data-testid="selected-kind">{selected?.kind ?? ""}</p>
    </>
  );
}

function renderHarness(initialPath = "/app/tickets/open") {
  const environment = createUnauthenticatedGraphQLEnvironment();
  return render(
    <MemoryRouter initialEntries={[initialPath]}>
      <RelayEnvironmentProvider environment={environment}>
        <Suspense fallback="loading">
          <Harness />
          <LocationProbe />
        </Suspense>
      </RelayEnvironmentProvider>
    </MemoryRouter>,
  );
}

describe("InstanceSwitcher", () => {
  it("defaults to the first membership when nothing is stored", async () => {
    mockMe();
    renderHarness();

    const select = await screen.findByRole("combobox", { name: "Instance" });
    await waitFor(() => expect(select).toHaveValue("inst-acme"));
  });

  it("persists a selection across a remount (reload)", async () => {
    mockMe();
    const user = UserEvent.setup();
    const { unmount } = renderHarness();

    const select = await screen.findByRole("combobox", { name: "Instance" });
    await waitFor(() => expect(select).toHaveValue("inst-acme"));

    await user.selectOptions(select, "inst-ridge");
    expect(select).toHaveValue("inst-ridge");

    unmount();

    renderHarness();
    const secondSelect = await screen.findByRole("combobox", {
      name: "Instance",
    });
    await waitFor(() => expect(secondSelect).toHaveValue("inst-ridge"));
  });

  it("renders a flat list when every instance is the same kind", async () => {
    mockMe();
    renderHarness();

    const select = await screen.findByRole("combobox", { name: "Instance" });
    await waitFor(() => expect(select).toHaveValue("inst-acme"));
    expect(select.querySelectorAll("optgroup")).toHaveLength(0);
    expect(select.querySelectorAll("option")).toHaveLength(2);
  });

  it("groups instances by kind when the user has both kinds", async () => {
    mockMe([...SUPPORT_MEMBERSHIPS, FINANCE_MEMBERSHIP]);
    renderHarness();

    const select = await screen.findByRole("combobox", { name: "Instance" });
    await waitFor(() => expect(select).toHaveValue("inst-acme"));
    const groups = select.querySelectorAll("optgroup");
    expect(Array.from(groups, (g) => g.label)).toEqual(["Support", "Finance"]);
    expect(
      Array.from(groups[0].querySelectorAll("option"), (o) => o.value),
    ).toEqual(["inst-acme", "inst-ridge"]);
    expect(
      Array.from(groups[1].querySelectorAll("option"), (o) => o.value),
    ).toEqual(["inst-ledger"]);
  });

  it("takes the kind from membership data, not storage", async () => {
    // Only the id was ever stored — including by builds that predate kinds.
    localStorage.setItem("mt_selected_instance_id", "inst-ledger");
    mockMe([...SUPPORT_MEMBERSHIPS, FINANCE_MEMBERSHIP]);
    renderHarness("/app/projects");

    const select = await screen.findByRole("combobox", { name: "Instance" });
    await waitFor(() => expect(select).toHaveValue("inst-ledger"));
    expect(screen.getByTestId("selected-kind")).toHaveTextContent("FINANCE");
  });

  it("navigates to the other kind's home when switching kind", async () => {
    mockMe([...SUPPORT_MEMBERSHIPS, FINANCE_MEMBERSHIP]);
    const user = UserEvent.setup();
    renderHarness("/app/tickets/all");

    const select = await screen.findByRole("combobox", { name: "Instance" });
    await waitFor(() => expect(select).toHaveValue("inst-acme"));

    // Same kind: stays on the current page.
    await user.selectOptions(select, "inst-ridge");
    expect(screen.getByTestId("location")).toHaveTextContent(
      "/app/tickets/all",
    );

    await user.selectOptions(select, "inst-ledger");
    expect(screen.getByTestId("location")).toHaveTextContent("/app/invoices");

    await user.selectOptions(select, "inst-acme");
    expect(screen.getByTestId("location")).toHaveTextContent(
      "/app/tickets/open",
    );
  });

  it("stays on a kind-neutral page when switching kind", async () => {
    mockMe([...SUPPORT_MEMBERSHIPS, FINANCE_MEMBERSHIP]);
    const user = UserEvent.setup();
    renderHarness("/app/settings");

    const select = await screen.findByRole("combobox", { name: "Instance" });
    await waitFor(() => expect(select).toHaveValue("inst-acme"));

    await user.selectOptions(select, "inst-ledger");
    expect(screen.getByTestId("selected-kind")).toHaveTextContent("FINANCE");
    expect(screen.getByTestId("location")).toHaveTextContent("/app/settings");
  });
});
