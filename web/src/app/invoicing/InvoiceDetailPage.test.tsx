import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import UserEvent from "@testing-library/user-event";
import { setupServer } from "msw/node";
import { graphql as mswGraphql, HttpResponse } from "msw";
import { RelayEnvironmentProvider } from "react-relay";
import { MemoryRouter, Route, Routes } from "react-router";
import { getGraphQLEndpoint } from "../../lib/api";
import { createUnauthenticatedGraphQLEnvironment } from "../../lib/environments";
import { SelectedInstanceContext } from "../SelectedInstanceContext";
import type { SelectedInstance } from "../selectedInstance";
import { localToday } from "../../lib/dates";
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

function invoiceResponse(
  overrides: Partial<{
    status: string;
    displayNumber: string | null;
    issueDate: string | null;
    paidDate: string | null;
    payments: unknown[];
    creditNotes: unknown[];
    gstRegistered: boolean;
    items: unknown[];
    subtotalCents: number;
    gstCents: number;
    totalCents: number;
  }> = {},
) {
  const status = overrides.status ?? "DRAFT";
  const gstRegistered = overrides.gstRegistered ?? false;
  const items = overrides.items ?? [item1];
  const subtotalCents = overrides.subtotalCents ?? 45000;
  const gstCents = overrides.gstCents ?? (gstRegistered ? 4500 : 0);
  const totalCents = overrides.totalCents ?? subtotalCents + gstCents;

  return {
    data: {
      invoice: {
        __typename: "Invoice",
        id: "inv-1",
        status,
        displayNumber: overrides.displayNumber ?? null,
        project: {
          __typename: "Project",
          id: "proj-1",
          name: "Website Redesign",
          clientEmail: "accounts@retail.example",
          effectivePaymentTermsDays: 30,
          instance: { __typename: "Instance", id: "inst-ledger" },
        },
        title: gstRegistered ? "Tax Invoice" : "Invoice",
        issueDate: overrides.issueDate ?? null,
        billTo,
        reference: "Shopfront site",
        seller,
        lines: items.map((item) => ({
          description: (item as typeof item1).description,
          quantity: (item as typeof item1).quantity,
          unitPriceCents: (item as typeof item1).unitPriceCents,
          amountCents: (item as typeof item1).amountCents,
          gstFree: false,
        })),
        subtotalCents,
        gstCents,
        totalCents,
        currency: "AUD",
        gstRegistered,
        paymentDetails: "BSB 000-000 Acc 00000000",
        items,
        paidDate: overrides.paidDate ?? null,
        dueDate: status === "FINALIZED" ? "2026-09-18" : null,
        overdue: false,
        daysOverdue: null,
        paidCents: overrides.paidDate ? totalCents : 0,
        creditedCents: 0,
        balanceCents:
          status === "FINALIZED" && !overrides.paidDate ? totalCents : 0,
        payments: overrides.payments ?? [],
        sentAt: null,
        sentTo: [],
        creditNotes: overrides.creditNotes ?? [],
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

describe("InvoiceDetailPage — draft", () => {
  it("shows draft actions and the finalize panel sends the issue date", async () => {
    let finalizeVariables: Record<string, unknown> | undefined;
    server.use(
      relayEndpoint.query("InvoiceDetailPageQuery", () =>
        HttpResponse.json(invoiceResponse()),
      ),
      relayEndpoint.mutation(
        "InvoiceDraftPanelFinalizeMutation",
        ({ variables }) => {
          finalizeVariables = variables;
          return HttpResponse.json({
            data: {
              finalizeInvoice: {
                __typename: "Invoice",
                id: "inv-1",
                status: "FINALIZED",
                items: [],
              },
            },
          });
        },
      ),
    );
    const user = UserEvent.setup();
    renderPage();

    expect(await screen.findByText("Draft invoice")).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Add items" }),
    ).toBeInTheDocument();
    expect(
      screen.getByRole("button", { name: "Delete draft" }),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Remove" })).toBeInTheDocument();
    // No payment control on a draft.
    expect(
      screen.queryByRole("button", { name: "Record payment" }),
    ).not.toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Finalize" }));
    expect(screen.getByLabelText("Issue date")).toHaveValue(localToday());
    await user.clear(screen.getByLabelText("Issue date"));
    await user.type(screen.getByLabelText("Issue date"), "2026-08-19");
    // Two "Finalize" buttons now exist: the toggle and the panel's submit.
    const finalizeButtons = screen.getAllByRole("button", { name: "Finalize" });
    await user.click(finalizeButtons[finalizeButtons.length - 1]);

    await waitFor(() =>
      expect(finalizeVariables).toEqual({
        invoiceId: "inv-1",
        issueDate: "2026-08-19",
        // The default follows the issue date: 30 days' terms.
        dueDate: "2026-09-18",
      }),
    );
  });

  it("shows the server's business-settings error with an owner link", async () => {
    server.use(
      relayEndpoint.query("InvoiceDetailPageQuery", () =>
        HttpResponse.json(invoiceResponse()),
      ),
      relayEndpoint.mutation("InvoiceDraftPanelFinalizeMutation", () =>
        HttpResponse.json({
          data: null,
          errors: [{ message: "Complete the invoicing settings first" }],
        }),
      ),
    );
    const user = UserEvent.setup();
    renderPage();

    await screen.findByText("Draft invoice");
    await user.click(screen.getByRole("button", { name: "Finalize" }));
    await user.click(screen.getAllByRole("button", { name: "Finalize" })[1]);

    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Complete the invoicing settings first",
    );
    expect(
      screen.getByRole("link", { name: "Business settings" }),
    ).toHaveAttribute("href", "/app/invoicing-settings");
  });

  it("deletes the draft and navigates to the invoices list", async () => {
    let deleteVariables: Record<string, unknown> | undefined;
    server.use(
      relayEndpoint.query("InvoiceDetailPageQuery", () =>
        HttpResponse.json(invoiceResponse()),
      ),
      relayEndpoint.mutation(
        "InvoiceDraftPanelDeleteMutation",
        ({ variables }) => {
          deleteVariables = variables;
          return HttpResponse.json({ data: { deleteInvoice: "inv-1" } });
        },
      ),
    );
    const user = UserEvent.setup();
    renderPage();

    await screen.findByText("Draft invoice");
    await user.click(screen.getByRole("button", { name: "Delete draft" }));
    await user.click(screen.getByRole("button", { name: "Confirm delete" }));

    await waitFor(() =>
      expect(deleteVariables).toEqual({ invoiceId: "inv-1" }),
    );
  });
});

describe("InvoiceDetailPage — finalized", () => {
  it("hides every draft action and records a payment of the balance", async () => {
    let paymentVariables: Record<string, unknown> | undefined;
    server.use(
      relayEndpoint.query("InvoiceDetailPageQuery", () =>
        HttpResponse.json(
          invoiceResponse({
            status: "FINALIZED",
            displayNumber: "008",
            issueDate: "2026-08-19",
          }),
        ),
      ),
      relayEndpoint.mutation(
        "InvoicePaidControlRecordMutation",
        ({ variables }) => {
          paymentVariables = variables;
          return HttpResponse.json({ data: null, errors: [{ message: "x" }] });
        },
      ),
    );
    const user = UserEvent.setup();
    renderPage();

    expect(await screen.findByText("Invoice 008")).toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Finalize" }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Add items" }),
    ).not.toBeInTheDocument();
    expect(
      screen.queryByRole("button", { name: "Delete draft" }),
    ).not.toBeInTheDocument();
    expect(screen.getByText("Due 18/09/2026")).toBeInTheDocument();

    expect(screen.getByLabelText("Payment date")).toHaveValue(localToday());
    expect(screen.getByLabelText("Amount (AUD)")).toHaveValue("450");
    await user.type(screen.getByLabelText("Note"), "EFT");
    await user.click(screen.getByRole("button", { name: "Record payment" }));

    await waitFor(() =>
      expect(paymentVariables).toEqual({
        invoiceId: "inv-1",
        input: { date: localToday(), amountCents: 45000, note: "EFT" },
      }),
    );
  });

  it("shows a settled invoice's payments and removes one", async () => {
    let deleteVariables: Record<string, unknown> | undefined;
    server.use(
      relayEndpoint.query("InvoiceDetailPageQuery", () =>
        HttpResponse.json(
          invoiceResponse({
            status: "FINALIZED",
            displayNumber: "008",
            issueDate: "2026-08-19",
            paidDate: "2026-08-25",
            payments: [
              {
                id: "pay-1",
                date: "2026-08-25",
                amountCents: 45000,
                note: null,
              },
            ],
          }),
        ),
      ),
      relayEndpoint.mutation(
        "InvoicePaidControlDeleteMutation",
        ({ variables }) => {
          deleteVariables = variables;
          return HttpResponse.json({ data: null, errors: [{ message: "x" }] });
        },
      ),
    );
    const user = UserEvent.setup();
    renderPage();

    expect(
      await screen.findByText("Paid — settled on 25/08/2026"),
    ).toBeInTheDocument();
    // Nothing owed: no form to record another payment.
    expect(
      screen.queryByRole("button", { name: "Record payment" }),
    ).not.toBeInTheDocument();
    await user.click(
      screen.getByRole("button", {
        name: "Remove payment of 450.00 on 25/08/2026",
      }),
    );

    await waitFor(() =>
      expect(deleteVariables).toEqual({
        invoiceId: "inv-1",
        paymentId: "pay-1",
      }),
    );
  });

  it("issues a full credit note", async () => {
    let creditVariables: Record<string, unknown> | undefined;
    server.use(
      relayEndpoint.query("InvoiceDetailPageQuery", () =>
        HttpResponse.json(
          invoiceResponse({
            status: "FINALIZED",
            displayNumber: "008",
            issueDate: "2026-08-19",
          }),
        ),
      ),
      relayEndpoint.mutation(
        "InvoiceCreditNotesIssueMutation",
        ({ variables }) => {
          creditVariables = variables;
          return HttpResponse.json({ data: null, errors: [{ message: "x" }] });
        },
      ),
    );
    const user = UserEvent.setup();
    renderPage();

    await screen.findByText("Invoice 008");
    await user.click(screen.getByRole("button", { name: "Issue credit note" }));
    await user.type(screen.getByLabelText("Reason"), "Cancelled");
    expect(screen.getByLabelText("The whole invoice")).toBeChecked();
    const issueButtons = screen.getAllByRole("button", {
      name: "Issue credit note",
    });
    await user.click(issueButtons[issueButtons.length - 1]);

    await waitFor(() =>
      expect(creditVariables).toEqual({
        invoiceId: "inv-1",
        input: { issueDate: localToday(), reason: "Cancelled", lines: null },
      }),
    );
  });

  it("emails the invoice to the client by default", async () => {
    let sendVariables: Record<string, unknown> | undefined;
    server.use(
      relayEndpoint.query("InvoiceDetailPageQuery", () =>
        HttpResponse.json(
          invoiceResponse({
            status: "FINALIZED",
            displayNumber: "008",
            issueDate: "2026-08-19",
          }),
        ),
      ),
      relayEndpoint.mutation("InvoiceSendPanelMutation", ({ variables }) => {
        sendVariables = variables;
        return HttpResponse.json({ data: null, errors: [{ message: "x" }] });
      }),
    );
    const user = UserEvent.setup();
    renderPage();

    await screen.findByText("Invoice 008");
    expect(screen.getByText("Not emailed yet.")).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Email to client" }));
    expect(
      screen.getByText(
        "Leave blank to send to the client email, accounts@retail.example.",
      ),
    ).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Send email" }));

    await waitFor(() =>
      expect(sendVariables).toEqual({
        invoiceId: "inv-1",
        input: { to: [], cc: [], message: null },
      }),
    );
  });
});

describe("InvoiceDetailPage — preview", () => {
  it("prints the no-GST note when the instance isn't GST-registered", async () => {
    server.use(
      relayEndpoint.query("InvoiceDetailPageQuery", () =>
        HttpResponse.json(invoiceResponse({ gstRegistered: false })),
      ),
    );
    renderPage();

    expect(await screen.findByText("Invoice")).toBeInTheDocument();
    expect(screen.getByText("No GST has been charged.")).toBeInTheDocument();
    expect(screen.getByText(/^Total AUD:/)).toBeInTheDocument();
    expect(screen.queryByText(/^GST \(10%\):/)).not.toBeInTheDocument();
  });

  it("prints Subtotal/GST/Total when the instance is GST-registered", async () => {
    server.use(
      relayEndpoint.query("InvoiceDetailPageQuery", () =>
        HttpResponse.json(invoiceResponse({ gstRegistered: true })),
      ),
    );
    renderPage();

    expect(await screen.findByText("Tax Invoice")).toBeInTheDocument();
    expect(screen.getByText(/^Subtotal AUD:/)).toBeInTheDocument();
    expect(screen.getByText(/^GST \(10%\):/)).toBeInTheDocument();
    expect(screen.getByText(/^Total AUD:/)).toBeInTheDocument();
    expect(
      screen.queryByText("No GST has been charged."),
    ).not.toBeInTheDocument();
  });
});
