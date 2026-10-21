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
import { FinanceSettingsForm } from "./FinanceSettingsForm";
import type { FinanceSettingsFormTestQuery } from "./__generated__/FinanceSettingsFormTestQuery.graphql";

const relayEndpoint = mswGraphql.link(getGraphQLEndpoint());
const server = setupServer();

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

const harnessQuery = graphql`
  query FinanceSettingsFormTestQuery @throwOnFieldError {
    instance(slug: "ledger") {
      ...FinanceSettingsForm_instance
    }
  }
`;

function Harness() {
  const data = useLazyLoadQuery<FinanceSettingsFormTestQuery>(harnessQuery, {});
  return data.instance ? (
    <FinanceSettingsForm instance={data.instance} />
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

const settings = {
  businessName: "Wattle Joinery",
  businessAbn: null,
  businessAddress: "1 Example Lane\nSampletown NSW 2000",
  businessPhone: null,
  businessEmail: "accounts@wattle.example",
  paymentDetails: null,
  gstRegistered: false,
  currency: "AUD",
  paymentTermsDays: 14,
};

function instanceResponse(financeSettings: typeof settings | null) {
  return {
    data: {
      instance: {
        __typename: "Instance",
        id: "inst-ledger",
        financeSettings,
      },
    },
  };
}

describe("FinanceSettingsForm", () => {
  it("submits every field as a full replace", async () => {
    let receivedVariables: Record<string, unknown> | undefined;
    server.use(
      relayEndpoint.query("FinanceSettingsFormTestQuery", () =>
        HttpResponse.json(instanceResponse(settings)),
      ),
      relayEndpoint.mutation("FinanceSettingsFormMutation", ({ variables }) => {
        receivedVariables = variables;
        return HttpResponse.json({
          data: {
            updateFinanceSettings: {
              __typename: "Instance",
              id: "inst-ledger",
              financeSettings: {
                ...settings,
                businessAbn: "12 345 678 901",
                paymentDetails: "BSB 000-000 Account 00000000",
                gstRegistered: true,
                currency: "NZD",
                paymentTermsDays: 14,
              },
            },
          },
        });
      }),
    );

    const user = UserEvent.setup();
    renderHarness();

    expect(await screen.findByLabelText("Business name")).toHaveValue(
      "Wattle Joinery",
    );
    await user.type(screen.getByLabelText("ABN"), "12 345 678 901");
    await user.type(
      screen.getByLabelText("Payment details"),
      "BSB 000-000 Account 00000000",
    );
    // Cleared fields are still sent — blank means "remove it".
    await user.clear(screen.getByLabelText("Email"));
    await user.click(
      screen.getByRole("checkbox", { name: "Registered for GST" }),
    );
    const currency = screen.getByLabelText("Currency");
    await user.clear(currency);
    await user.type(currency, "nzd");
    await user.click(screen.getByRole("button", { name: "Save" }));

    expect(await screen.findByText("Saved.")).toBeInTheDocument();
    expect(receivedVariables).toEqual({
      instanceId: "inst-ledger",
      input: {
        businessName: "Wattle Joinery",
        businessAbn: "12 345 678 901",
        businessAddress: "1 Example Lane\nSampletown NSW 2000",
        businessPhone: "",
        businessEmail: "",
        paymentDetails: "BSB 000-000 Account 00000000",
        gstRegistered: true,
        currency: "NZD",
        paymentTermsDays: 14,
      },
    });
  });

  it("shows the server's error inline", async () => {
    server.use(
      relayEndpoint.query("FinanceSettingsFormTestQuery", () =>
        HttpResponse.json(instanceResponse(settings)),
      ),
      relayEndpoint.mutation("FinanceSettingsFormMutation", () =>
        HttpResponse.json({
          data: null,
          errors: [{ message: "Forbidden" }],
        }),
      ),
    );

    const user = UserEvent.setup();
    renderHarness();

    await screen.findByLabelText("Business name");
    await user.click(screen.getByRole("button", { name: "Save" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("Forbidden");
  });

  it("renders nothing for a support instance", async () => {
    server.use(
      relayEndpoint.query("FinanceSettingsFormTestQuery", () =>
        HttpResponse.json(instanceResponse(null)),
      ),
    );
    renderHarness();
    await waitFor(() =>
      expect(screen.queryByText("loading")).not.toBeInTheDocument(),
    );
    expect(screen.queryByLabelText("Business name")).not.toBeInTheDocument();
  });
});
