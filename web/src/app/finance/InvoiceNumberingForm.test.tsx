import { Suspense } from "react";
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
import { getGraphQLEndpoint } from "../../lib/api";
import { createUnauthenticatedGraphQLEnvironment } from "../../lib/environments";
import { InvoiceNumberingForm } from "./InvoiceNumberingForm";
import type { InvoiceNumberingFormTestQuery } from "./__generated__/InvoiceNumberingFormTestQuery.graphql";

const relayEndpoint = mswGraphql.link(getGraphQLEndpoint());
const server = setupServer();

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

const harnessQuery = graphql`
  query InvoiceNumberingFormTestQuery @throwOnFieldError {
    instance(slug: "ledger") {
      ...InvoiceNumberingForm_instance
    }
  }
`;

function Harness() {
  const data = useLazyLoadQuery<InvoiceNumberingFormTestQuery>(
    harnessQuery,
    {},
  );
  return data.instance ? (
    <InvoiceNumberingForm instance={data.instance} />
  ) : null;
}

function renderHarness() {
  const environment = createUnauthenticatedGraphQLEnvironment();
  return render(
    <RelayEnvironmentProvider environment={environment}>
      <Suspense fallback="loading">
        <Harness />
      </Suspense>
    </RelayEnvironmentProvider>,
  );
}

function instanceResponse(nextInvoiceNumber: number) {
  return {
    data: {
      instance: {
        __typename: "Instance",
        id: "inst-ledger",
        financeSettings: { nextInvoiceNumber },
      },
    },
  };
}

describe("InvoiceNumberingForm", () => {
  it("shows the current next number and submits an update", async () => {
    let receivedVariables: Record<string, unknown> | undefined;
    server.use(
      relayEndpoint.query("InvoiceNumberingFormTestQuery", () =>
        HttpResponse.json(instanceResponse(9)),
      ),
      relayEndpoint.mutation(
        "InvoiceNumberingFormMutation",
        ({ variables }) => {
          receivedVariables = variables;
          return HttpResponse.json({
            data: {
              setNextInvoiceNumber: {
                __typename: "Instance",
                id: "inst-ledger",
                financeSettings: { nextInvoiceNumber: 20 },
              },
            },
          });
        },
      ),
    );
    const user = UserEvent.setup();
    renderHarness();

    const input = await screen.findByLabelText("Next invoice number");
    expect(input).toHaveValue(9);
    await user.clear(input);
    await user.type(input, "20");
    await user.click(screen.getByRole("button", { name: "Update" }));

    await waitFor(() =>
      expect(receivedVariables).toEqual({
        instanceId: "inst-ledger",
        next: 20,
      }),
    );
    expect(await screen.findByText("Saved.")).toBeInTheDocument();
    expect(input).toHaveValue(20);
  });

  it("shows the server's CONFLICT error when it can't move backward", async () => {
    server.use(
      relayEndpoint.query("InvoiceNumberingFormTestQuery", () =>
        HttpResponse.json(instanceResponse(9)),
      ),
      relayEndpoint.mutation("InvoiceNumberingFormMutation", () =>
        HttpResponse.json({
          data: null,
          errors: [
            { message: "The next invoice number can only move forward" },
          ],
        }),
      ),
    );
    const user = UserEvent.setup();
    renderHarness();

    const input = await screen.findByLabelText("Next invoice number");
    await user.clear(input);
    await user.type(input, "1");
    await user.click(screen.getByRole("button", { name: "Update" }));

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "The next invoice number can only move forward",
    );
  });

  it("rejects a non-numeric value client-side without calling the API", async () => {
    server.use(
      relayEndpoint.query("InvoiceNumberingFormTestQuery", () =>
        HttpResponse.json(instanceResponse(9)),
      ),
    );
    const user = UserEvent.setup();
    renderHarness();

    const input = await screen.findByLabelText("Next invoice number");
    await user.clear(input);
    await user.click(screen.getByRole("button", { name: "Update" }));

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Enter a whole number of 1 or more.",
    );
  });
});
