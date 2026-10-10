import {
  afterAll,
  afterEach,
  beforeAll,
  describe,
  expect,
  it,
  vi,
} from "vitest";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import UserEvent from "@testing-library/user-event";
import { setupServer } from "msw/node";
import { graphql as mswGraphql, HttpResponse } from "msw";
import { RelayEnvironmentProvider } from "react-relay";
import { MemoryRouter, Route, Routes } from "react-router";
import { getGraphQLEndpoint } from "../../lib/api";
import { createUnauthenticatedGraphQLEnvironment } from "../../lib/environments";
import { SelectedInstanceContext } from "../SelectedInstanceContext";
import type { SelectedInstance } from "../selectedInstance";
import { InvoiceDetailPage } from "./InvoiceDetailPage";

const relayEndpoint = mswGraphql.link(getGraphQLEndpoint());
const server = setupServer();

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

const instance: SelectedInstance = {
  id: "inst-ledger",
  slug: "ledger",
  name: "Ledger & Co Bookkeeping",
  role: "OWNER",
  kind: "INVOICING",
};

const seller = {
  name: "Fictional Trades Pty Ltd",
  abn: "11 222 333 444",
  address: "1 Fictional St\nSomewhere NSW 2000",
  phone: "0400 000 000",
  email: "billing@fictional.example",
};

const billTo = {
  name: "Fictional Retail Pty Ltd",
  abn: "55 666 777 888",
  address: "22 Example Street\nExampleville",
};

const item1 = {
  __typename: "BillableItem",
  id: "item-1",
  date: "2026-08-03",
  description: "Discovery workshop",
  quantity: "3",
  unitPriceCents: 15000,
  amountCents: 45000,
  gstFree: false,
  status: "DRAFT",
  invoice: { __typename: "Invoice", id: "inv-1" },
  project: { __typename: "Project", id: "proj-1", name: "Website Redesign" },
};

function invoiceResponse(status: string) {
  return {
    data: {
      invoice: {
        __typename: "Invoice",
        id: "inv-1",
        status,
        displayNumber: status === "FINALIZED" ? "008" : null,
        project: {
          __typename: "Project",
          id: "proj-1",
          name: "Website Redesign",
          clientEmail: null,
          effectivePaymentTermsDays: 14,
          instance: { __typename: "Instance", id: "inst-ledger" },
        },
        title: "Invoice",
        issueDate: status === "FINALIZED" ? "2026-08-19" : null,
        billTo,
        reference: "Shopfront site",
        seller,
        lines: [
          {
            description: item1.description,
            quantity: item1.quantity,
            unitPriceCents: item1.unitPriceCents,
            amountCents: item1.amountCents,
            gstFree: false,
          },
        ],
        subtotalCents: 45000,
        gstCents: 0,
        totalCents: 45000,
        currency: "AUD",
        gstRegistered: false,
        paymentDetails: "BSB 000-000 Acc 00000000",
        items: [item1],
        paidDate: null,
        dueDate: status === "FINALIZED" ? "2026-09-02" : null,
        overdue: false,
        daysOverdue: null,
        paidCents: 0,
        creditedCents: 0,
        balanceCents: status === "FINALIZED" ? 45000 : 0,
        payments: [],
        sentAt: null,
        sentTo: [],
        creditNotes: [],
      },
    },
  };
}

function renderPage() {
  const environment = createUnauthenticatedGraphQLEnvironment();
  return render(
    <MemoryRouter initialEntries={["/app/invoices/inv-1"]}>
      <RelayEnvironmentProvider environment={environment}>
        <SelectedInstanceContext value={instance}>
          <Routes>
            <Route path="/app/invoices/:id" element={<InvoiceDetailPage />} />
          </Routes>
        </SelectedInstanceContext>
      </RelayEnvironmentProvider>
    </MemoryRouter>,
  );
}

describe("InvoiceDownloadButton", () => {
  it("is absent on a draft invoice", async () => {
    server.use(
      relayEndpoint.query("InvoiceDetailPageQuery", () =>
        HttpResponse.json(invoiceResponse("DRAFT")),
      ),
    );
    renderPage();

    await screen.findByText("Draft invoice");
    expect(
      screen.queryByRole("button", { name: "Download PDF" }),
    ).not.toBeInTheDocument();
  });

  it("sends the invoice id and navigates to the returned URL on success", async () => {
    let receivedVariables: Record<string, unknown> | undefined;
    server.use(
      relayEndpoint.query("InvoiceDetailPageQuery", () =>
        HttpResponse.json(invoiceResponse("FINALIZED")),
      ),
      relayEndpoint.mutation(
        "InvoiceDownloadButtonMutation",
        async ({ variables }) => {
          receivedVariables = variables;
          // A small delay keeps the pending state observable below —
          // otherwise msw's response can resolve before the assertion runs.
          await new Promise((resolve) => setTimeout(resolve, 20));
          return HttpResponse.json({
            data: {
              downloadInvoicePdf: "https://s3.example/invoices/inv-1.pdf",
            },
          });
        },
      ),
    );
    const assign = vi.fn();
    vi.spyOn(window, "location", "get").mockReturnValue({
      ...window.location,
      assign,
    } as unknown as Location);
    renderPage();

    await screen.findByText("Invoice 008");
    const button = screen.getByRole("button", { name: "Download PDF" });
    fireEvent.click(button);
    expect(
      await screen.findByRole("button", { name: "Preparing PDF…" }),
    ).toBeDisabled();

    await waitFor(() =>
      expect(receivedVariables).toEqual({ invoiceId: "inv-1" }),
    );
    await waitFor(() =>
      expect(assign).toHaveBeenCalledWith(
        "https://s3.example/invoices/inv-1.pdf",
      ),
    );
    vi.restoreAllMocks();
  });

  it("shows the server's error on failure", async () => {
    server.use(
      relayEndpoint.query("InvoiceDetailPageQuery", () =>
        HttpResponse.json(invoiceResponse("FINALIZED")),
      ),
      relayEndpoint.mutation("InvoiceDownloadButtonMutation", () =>
        HttpResponse.json({
          data: null,
          errors: [{ message: "Invoice is not finalized" }],
        }),
      ),
    );
    const user = UserEvent.setup();
    renderPage();

    await screen.findByText("Invoice 008");
    await user.click(screen.getByRole("button", { name: "Download PDF" }));

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Invoice is not finalized",
    );
  });
});
