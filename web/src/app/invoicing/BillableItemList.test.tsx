import { Suspense, useState } from "react";
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { render, screen, waitFor, within } from "@testing-library/react";
import UserEvent from "@testing-library/user-event";
import { setupServer } from "msw/node";
import { graphql as mswGraphql, HttpResponse } from "msw";
import {
  graphql,
  useLazyLoadQuery,
  RelayEnvironmentProvider,
} from "react-relay";
import { MemoryRouter } from "react-router";
import { getGraphQLEndpoint } from "../../lib/api";
import { createUnauthenticatedGraphQLEnvironment } from "../../lib/environments";
import { BillableItemList } from "./BillableItemList";
import type {
  BillableItemFilterType,
  BillableItemListTestQuery,
} from "./__generated__/BillableItemListTestQuery.graphql";

const relayEndpoint = mswGraphql.link(getGraphQLEndpoint());
const server = setupServer();

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

const harnessQuery = graphql`
  query BillableItemListTestQuery(
    $instanceId: ID!
    $projectId: ID
    $filter: BillableItemFilterType!
  ) @throwOnFieldError {
    ...BillableItemList_query
      @arguments(
        instanceId: $instanceId
        projectId: $projectId
        filter: $filter
      )
  }
`;

function itemNode(overrides: Partial<ReturnType<typeof baseItem>> = {}) {
  return { ...baseItem(), ...overrides };
}

function baseItem() {
  return {
    __typename: "BillableItem",
    id: "item-1",
    date: "2026-08-19",
    description: "Front-end build",
    quantity: "12.5",
    unitPriceCents: 9876,
    amountCents: 123450,
    gstFree: false,
    status: "UNBILLED",
    invoice: null,
    project: {
      __typename: "Project",
      id: "proj-1",
      name: "Website Redesign",
    },
  };
}

function connection(nodes: Array<ReturnType<typeof itemNode>>) {
  return {
    edges: nodes.map((node) => ({ node, cursor: `${node.date}:${node.id}` })),
    pageInfo: {
      hasNextPage: false,
      hasPreviousPage: false,
      startCursor: null,
      endCursor: null,
    },
  };
}

function Harness({
  filter,
  editable,
}: {
  filter: BillableItemFilterType;
  editable: boolean;
}) {
  const [initialFilter] = useState(filter);
  const data = useLazyLoadQuery<BillableItemListTestQuery>(harnessQuery, {
    instanceId: "inst-1",
    projectId: editable ? "proj-1" : null,
    filter: initialFilter,
  });
  return (
    <BillableItemList
      query={data}
      filter={filter}
      showProject={!editable}
      editable={editable}
      currency="AUD"
      emptyMessage="Nothing here"
    />
  );
}

function renderHarness({ editable = false } = {}) {
  const environment = createUnauthenticatedGraphQLEnvironment();
  function Wrapper() {
    const [filter, setFilter] = useState<BillableItemFilterType>("ALL");
    return (
      <>
        <button onClick={() => setFilter("UNBILLED")}>Show unbilled</button>
        <Suspense fallback="loading">
          <Harness filter={filter} editable={editable} />
        </Suspense>
      </>
    );
  }
  return render(
    <MemoryRouter>
      <RelayEnvironmentProvider environment={environment}>
        <Wrapper />
      </RelayEnvironmentProvider>
    </MemoryRouter>,
  );
}

describe("BillableItemList", () => {
  it("renders a row's date, project link, quantity and formatted money", async () => {
    server.use(
      relayEndpoint.query("BillableItemListTestQuery", () =>
        HttpResponse.json({
          data: { billableItems: connection([itemNode()]) },
        }),
      ),
    );
    renderHarness();

    expect(await screen.findByText("Front-end build")).toBeInTheDocument();
    const row = screen.getByRole("listitem");
    expect(within(row).getByText("19/08/2026")).toBeInTheDocument();
    expect(
      within(row).getByRole("link", { name: "Website Redesign" }),
    ).toHaveAttribute("href", "/app/projects/proj-1");
    expect(within(row).getByText("12.5")).toBeInTheDocument();
    expect(within(row).getByText("98.76")).toBeInTheDocument();
    expect(within(row).getByText("1,234.50")).toBeInTheDocument();
    expect(within(row).getByText("Unbilled")).toBeInTheDocument();
    // Read-only list: no edit controls.
    expect(
      within(row).queryByRole("button", { name: "Edit" }),
    ).not.toBeInTheDocument();
  });

  it("shows the first line of a multi-line description until expanded", async () => {
    server.use(
      relayEndpoint.query("BillableItemListTestQuery", () =>
        HttpResponse.json({
          data: {
            billableItems: connection([
              itemNode({ description: "Designs\n* homepage\n* product page" }),
            ]),
          },
        }),
      ),
    );
    const user = UserEvent.setup();
    renderHarness();

    expect(await screen.findByText("Designs")).toBeInTheDocument();
    expect(screen.queryByText(/homepage/)).not.toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Show all 3 lines" }));
    expect(screen.getByText(/\* homepage/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Show less" })).toHaveAttribute(
      "aria-expanded",
      "true",
    );
  });

  it("refetches with the new filter instead of re-running the page query", async () => {
    let pageCalls = 0;
    let refetchFilter: unknown;
    server.use(
      relayEndpoint.query("BillableItemListTestQuery", ({ variables }) => {
        pageCalls += 1;
        expect(variables.filter).toBe("ALL");
        return HttpResponse.json({
          data: {
            billableItems: connection([
              itemNode({
                id: "a",
                description: "Billed thing",
                status: "INVOICED",
              }),
              itemNode({ id: "b", description: "Unbilled thing" }),
            ]),
          },
        });
      }),
      relayEndpoint.query(
        "BillableItemListPaginationQuery",
        ({ variables }) => {
          refetchFilter = variables.filter;
          return HttpResponse.json({
            data: {
              billableItems: connection([
                itemNode({ id: "b", description: "Unbilled thing" }),
              ]),
            },
          });
        },
      ),
    );
    const user = UserEvent.setup();
    renderHarness();

    expect(await screen.findByText("Billed thing")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Show unbilled" }));

    await waitFor(() =>
      expect(screen.queryByText("Billed thing")).not.toBeInTheDocument(),
    );
    expect(screen.getByText("Unbilled thing")).toBeInTheDocument();
    expect(refetchFilter).toBe("UNBILLED");
    expect(pageCalls).toBe(1);
  });

  it("renders the empty message", async () => {
    server.use(
      relayEndpoint.query("BillableItemListTestQuery", () =>
        HttpResponse.json({ data: { billableItems: connection([]) } }),
      ),
    );
    renderHarness();
    expect(await screen.findByText("Nothing here")).toBeInTheDocument();
  });

  it("edits an unbilled item inline, sending the quantity string and integer cents", async () => {
    let updateVariables: Record<string, unknown> | undefined;
    server.use(
      relayEndpoint.query("BillableItemListTestQuery", () =>
        HttpResponse.json({
          data: {
            billableItems: connection([
              itemNode(),
              itemNode({
                id: "billed",
                description: "Already invoiced",
                status: "INVOICED",
              }),
            ]),
          },
        }),
      ),
      relayEndpoint.mutation(
        "BillableItemRowUpdateMutation",
        ({ variables }) => {
          updateVariables = variables;
          return HttpResponse.json({
            data: {
              updateBillableItem: itemNode({
                quantity: "2",
                unitPriceCents: 150000,
                amountCents: 300000,
              }),
            },
          });
        },
      ),
      // The list refetches after a successful edit.
      relayEndpoint.query("BillableItemListPaginationQuery", () =>
        HttpResponse.json({
          data: {
            billableItems: connection([
              itemNode({
                quantity: "2",
                unitPriceCents: 150000,
                amountCents: 300000,
              }),
            ]),
          },
        }),
      ),
    );
    const user = UserEvent.setup();
    renderHarness({ editable: true });

    await screen.findByText("Front-end build");
    // Only the unbilled row is editable.
    expect(screen.getAllByRole("button", { name: "Edit" })).toHaveLength(1);
    await user.click(screen.getByRole("button", { name: "Edit" }));

    const quantity = screen.getByLabelText("Quantity");
    expect(quantity).toHaveValue("12.5");
    expect(screen.getByLabelText("Unit price (AUD)")).toHaveValue("98.76");
    await user.clear(quantity);
    await user.type(quantity, "2");
    await user.clear(screen.getByLabelText("Unit price (AUD)"));
    await user.type(screen.getByLabelText("Unit price (AUD)"), "1,500");
    expect(screen.getByTestId("edit-item-1-amount")).toHaveTextContent(
      "3,000.00 AUD",
    );
    await user.click(screen.getByRole("button", { name: "Save" }));

    await waitFor(() => expect(updateVariables).toBeDefined());
    expect(updateVariables).toEqual({
      id: "item-1",
      input: {
        date: "2026-08-19",
        description: "Front-end build",
        quantity: "2",
        unitPriceCents: 150000,
        gstFree: false,
      },
    });
    expect(await screen.findByText("3,000.00")).toBeInTheDocument();
  });

  it("deletes after a second click and drops the row", async () => {
    let deleteVariables: Record<string, unknown> | undefined;
    server.use(
      relayEndpoint.query("BillableItemListTestQuery", () =>
        HttpResponse.json({
          data: { billableItems: connection([itemNode()]) },
        }),
      ),
      relayEndpoint.mutation(
        "BillableItemRowDeleteMutation",
        ({ variables }) => {
          deleteVariables = variables;
          return HttpResponse.json({
            data: { deleteBillableItem: "item-1" },
          });
        },
      ),
    );
    const user = UserEvent.setup();
    renderHarness({ editable: true });

    await screen.findByText("Front-end build");
    await user.click(screen.getByRole("button", { name: "Delete" }));
    expect(deleteVariables).toBeUndefined();
    await user.click(screen.getByRole("button", { name: "Confirm delete" }));

    expect(await screen.findByText("Nothing here")).toBeInTheDocument();
    expect(deleteVariables?.id).toBe("item-1");
  });
});
