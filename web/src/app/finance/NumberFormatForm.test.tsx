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
import { NumberFormatForm } from "./NumberFormatForm";
import type { NumberFormatFormTestQuery } from "./__generated__/NumberFormatFormTestQuery.graphql";

const relayEndpoint = mswGraphql.link(getGraphQLEndpoint());
const server = setupServer();

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

const harnessQuery = graphql`
  query NumberFormatFormTestQuery @throwOnFieldError {
    instance(slug: "ledger") {
      ...NumberFormatForm_instance
    }
  }
`;

function Harness() {
  const data = useLazyLoadQuery<NumberFormatFormTestQuery>(harnessQuery, {});
  return data.instance ? <NumberFormatForm instance={data.instance} /> : null;
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

const settings = (overrides: Record<string, unknown> = {}) => ({
  __typename: "Instance",
  id: "inst-ledger",
  financeSettings: {
    invoiceNumberPrefix: "",
    creditNoteNumberPrefix: "CN-",
    numberPadding: 3,
    nextCreditNoteNumber: 1,
    ...overrides,
  },
});

describe("NumberFormatForm", () => {
  it("previews the format and saves it, then the credit note number", async () => {
    let formatVars: Record<string, unknown> | undefined;
    let nextVars: Record<string, unknown> | undefined;
    server.use(
      relayEndpoint.query("NumberFormatFormTestQuery", () =>
        HttpResponse.json({ data: { instance: settings() } }),
      ),
      relayEndpoint.mutation("NumberFormatFormMutation", ({ variables }) => {
        formatVars = variables;
        return HttpResponse.json({
          data: {
            setNumberFormat: settings({
              invoiceNumberPrefix: "INV-",
              numberPadding: 4,
            }),
          },
        });
      }),
      relayEndpoint.mutation(
        "NumberFormatFormNextMutation",
        ({ variables }) => {
          nextVars = variables;
          return HttpResponse.json({
            data: {
              setNextCreditNoteNumber: settings({
                invoiceNumberPrefix: "INV-",
                numberPadding: 4,
                nextCreditNoteNumber: 12,
              }),
            },
          });
        },
      ),
    );
    const user = UserEvent.setup();
    renderHarness();

    await user.type(await screen.findByLabelText("Invoice prefix"), "INV-");
    await user.clear(screen.getByLabelText("Digits"));
    await user.type(screen.getByLabelText("Digits"), "4");
    expect(screen.getByText("INV-0042")).toBeInTheDocument();
    expect(screen.getByText("CN-0007")).toBeInTheDocument();
    await user.clear(screen.getByLabelText("Next credit note number"));
    await user.type(screen.getByLabelText("Next credit note number"), "12");
    await user.click(screen.getByRole("button", { name: "Save numbering" }));

    await waitFor(() =>
      expect(nextVars).toEqual({ instanceId: "inst-ledger", next: 12 }),
    );
    expect(formatVars).toEqual({
      instanceId: "inst-ledger",
      invoicePrefix: "INV-",
      creditNotePrefix: "CN-",
      padding: 4,
    });
    expect(await screen.findByText("Saved.")).toBeInTheDocument();
  });

  it("refuses padding outside 1 to 8 without calling the API", async () => {
    server.use(
      relayEndpoint.query("NumberFormatFormTestQuery", () =>
        HttpResponse.json({ data: { instance: settings() } }),
      ),
    );
    const user = UserEvent.setup();
    renderHarness();
    const digits = await screen.findByLabelText("Digits");
    await user.clear(digits);
    await user.click(screen.getByRole("button", { name: "Save numbering" }));
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Padding must be a whole number from 1 to 8.",
    );
  });
});
