import {
  afterAll,
  afterEach,
  beforeAll,
  describe,
  expect,
  it,
  vi,
} from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import UserEvent from "@testing-library/user-event";
import { setupServer } from "msw/node";
import { graphql as mswGraphql, HttpResponse } from "msw";
import { RelayEnvironmentProvider } from "react-relay";
import { MemoryRouter } from "react-router";
import { getGraphQLEndpoint } from "../../lib/api";
import { createUnauthenticatedGraphQLEnvironment } from "../../lib/environments";
import { localToday } from "../../lib/dates";
import {
  AddBillableItemForm,
  ProjectBillableItems,
} from "./ProjectBillableItems";

const relayEndpoint = mswGraphql.link(getGraphQLEndpoint());
const server = setupServer();

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

function renderForm(onCreated = vi.fn()) {
  const environment = createUnauthenticatedGraphQLEnvironment();
  render(
    <RelayEnvironmentProvider environment={environment}>
      <AddBillableItemForm
        projectId="proj-1"
        currency="NZD"
        onCreated={onCreated}
      />
    </RelayEnvironmentProvider>,
  );
  return onCreated;
}

describe("AddBillableItemForm", () => {
  it("submits the quantity as a string and the price as integer cents", async () => {
    let received: Record<string, unknown> | undefined;
    server.use(
      relayEndpoint.mutation(
        "ProjectBillableItemsCreateMutation",
        ({ variables }) => {
          received = variables;
          return HttpResponse.json({
            data: {
              createBillableItem: { __typename: "BillableItem", id: "new-1" },
            },
          });
        },
      ),
    );
    const user = UserEvent.setup();
    const onCreated = renderForm();

    // Defaults to today, in local time.
    expect(screen.getByLabelText("Date")).toHaveValue(localToday());
    await user.clear(screen.getByLabelText("Date"));
    await user.type(screen.getByLabelText("Date"), "2026-08-19");
    await user.type(
      screen.getByLabelText("Description"),
      "Site visit{enter}* measure up",
    );
    await user.type(screen.getByLabelText("Quantity"), "1.5");
    await user.type(screen.getByLabelText("Unit price (NZD)"), "1,200.50");
    // 1.5 × 1,200.50 = 1,800.75
    expect(screen.getByText("1,800.75 NZD")).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Add item" }));

    await waitFor(() => expect(onCreated).toHaveBeenCalledTimes(1));
    expect(received).toEqual({
      projectId: "proj-1",
      input: {
        date: "2026-08-19",
        description: "Site visit\n* measure up",
        quantity: "1.5",
        unitPriceCents: 120050,
        gstFree: false,
      },
    });
    // The form resets for the next item but keeps the date just used.
    expect(screen.getByLabelText("Description")).toHaveValue("");
    expect(screen.getByLabelText("Quantity")).toHaveValue("");
    expect(screen.getByLabelText("Date")).toHaveValue("2026-08-19");
  });

  it("refuses an unparseable price without calling the API", async () => {
    const user = UserEvent.setup();
    const onCreated = renderForm();

    await user.type(screen.getByLabelText("Description"), "Work");
    await user.type(screen.getByLabelText("Quantity"), "1");
    await user.type(screen.getByLabelText("Unit price (NZD)"), "12.345");
    expect(screen.getByText("—")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Add item" }));

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "at most 2 decimal places",
    );
    expect(onCreated).not.toHaveBeenCalled();
  });

  it("shows the API's error message", async () => {
    server.use(
      relayEndpoint.mutation("ProjectBillableItemsCreateMutation", () =>
        HttpResponse.json({
          data: null,
          errors: [
            {
              message: "Quantity can have at most 2 decimal places",
              extensions: { code: "INTERNAL" },
            },
          ],
        }),
      ),
    );
    const user = UserEvent.setup();
    const onCreated = renderForm();

    await user.type(screen.getByLabelText("Description"), "Work");
    await user.type(screen.getByLabelText("Quantity"), "1.234");
    await user.type(screen.getByLabelText("Unit price (NZD)"), "10");
    await user.click(screen.getByRole("button", { name: "Add item" }));

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Quantity can have at most 2 decimal places",
    );
    expect(onCreated).not.toHaveBeenCalled();
  });
});

function unbilledItem(id: string, description: string) {
  return {
    __typename: "BillableItem",
    id,
    date: "2026-08-19",
    description,
    quantity: "1",
    unitPriceCents: 10000,
    amountCents: 10000,
    gstFree: false,
    status: "UNBILLED",
    invoice: null,
    project: { __typename: "Project", id: "proj-1", name: "Website Redesign" },
  };
}

describe("ProjectBillableItems — create invoice from selected", () => {
  it("sends only the checked unbilled items and navigates to the new invoice", async () => {
    let createVariables: Record<string, unknown> | undefined;
    server.use(
      relayEndpoint.query("ProjectBillableItemsQuery", () =>
        HttpResponse.json({
          data: {
            billableItems: {
              edges: [
                {
                  node: unbilledItem("item-1", "Discovery workshop"),
                  cursor: "1",
                },
                {
                  node: unbilledItem("item-2", "Front-end build"),
                  cursor: "2",
                },
              ],
              pageInfo: {
                hasNextPage: false,
                hasPreviousPage: false,
                startCursor: null,
                endCursor: null,
              },
            },
          },
        }),
      ),
      relayEndpoint.mutation(
        "ProjectBillableItemsCreateInvoiceMutation",
        ({ variables }) => {
          createVariables = variables;
          return HttpResponse.json({
            data: {
              createInvoice: {
                __typename: "Invoice",
                id: "inv-new",
                items: [],
              },
            },
          });
        },
      ),
    );
    const user = UserEvent.setup();
    render(
      <MemoryRouter>
        <RelayEnvironmentProvider
          environment={createUnauthenticatedGraphQLEnvironment()}
        >
          <ProjectBillableItems
            instanceId="inst-1"
            projectId="proj-1"
            currency="AUD"
            archived={false}
          />
        </RelayEnvironmentProvider>
      </MemoryRouter>,
    );

    await screen.findByText("Discovery workshop");
    // Not shown until something is selected.
    expect(
      screen.queryByRole("button", { name: /Create invoice from selected/ }),
    ).not.toBeInTheDocument();

    await user.click(
      screen.getAllByRole("checkbox", { name: "Select for invoice" })[0],
    );
    const createButton = screen.getByRole("button", {
      name: "Create invoice from selected (1)",
    });
    await user.click(createButton);

    await waitFor(() =>
      expect(createVariables).toEqual({
        projectId: "proj-1",
        itemIds: ["item-1"],
      }),
    );
  });
});
