import {
  afterAll,
  afterEach,
  beforeAll,
  describe,
  expect,
  it,
  vi,
} from "vitest";
import { render, screen, fireEvent, waitFor } from "@testing-library/react";
import { setupServer } from "msw/node";
import { graphql as mswGraphql, HttpResponse } from "msw";
import { RelayEnvironmentProvider } from "react-relay";
import { getGraphQLEndpoint } from "../../lib/api";
import { createUnauthenticatedGraphQLEnvironment } from "../../lib/environments";
import { BroadcastForm } from "./BroadcastForm";

const relayEndpoint = mswGraphql.link(getGraphQLEndpoint());
const server = setupServer();

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

function renderForm(onSent = vi.fn(), domains = ["seslogin.test"]) {
  render(
    <RelayEnvironmentProvider
      environment={createUnauthenticatedGraphQLEnvironment()}
    >
      <BroadcastForm
        instanceId="inst-1"
        defaultFromName="seslogin Support"
        domains={domains}
        onSent={onSent}
      />
    </RelayEnvironmentProvider>,
  );
  return onSent;
}

function fill() {
  fireEvent.change(screen.getByLabelText("Recipients"), {
    target: { value: "Alice@example.com\nbob@example.com, alice@example.com" },
  });
  fireEvent.change(screen.getByLabelText("From address"), {
    target: { value: "news" },
  });
  fireEvent.change(screen.getByLabelText("Subject"), {
    target: { value: "New in seslogin" },
  });
  fireEvent.change(screen.getByLabelText("Message"), {
    target: { value: "We shipped a thing." },
  });
}

describe("BroadcastForm", () => {
  it("counts de-duplicated recipients and reviews before sending", async () => {
    let variables: Record<string, unknown> | undefined;
    server.use(
      relayEndpoint.mutation("BroadcastFormMutation", ({ variables: v }) => {
        variables = v;
        return HttpResponse.json({
          data: {
            sendBroadcast: {
              id: "b-1",
              subject: "New in seslogin",
              fromAddress: "news@seslogin.test",
              recipientCount: 2,
              sentCount: 1,
              recipients: [
                { email: "alice@example.com", sendStatus: "SENT", error: null },
                {
                  email: "bob@example.com",
                  sendStatus: "FAILED",
                  error: "Address blacklisted",
                },
              ],
            },
          },
        });
      }),
    );
    const onSent = renderForm();
    fill();
    expect(screen.getByText(/2 addresses/)).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "Review" }));
    expect(screen.getByText("Send to 2 recipients?")).toBeTruthy();
    expect(
      screen.getByText("seslogin Support <news@seslogin.test>"),
    ).toBeTruthy();
    expect(variables).toBeUndefined();

    fireEvent.click(screen.getByRole("button", { name: "Send now" }));
    await waitFor(() =>
      expect(screen.getByRole("status").textContent).toContain(
        "to 1 of 2 recipients",
      ),
    );
    expect(screen.getByText(/Address blacklisted/)).toBeTruthy();
    expect(onSent).toHaveBeenCalledOnce();
    expect(variables).toEqual({
      instanceId: "inst-1",
      input: {
        fromAddress: "news@seslogin.test",
        fromName: null,
        subject: "New in seslogin",
        body: "We shipped a thing.",
        recipients: ["alice@example.com", "bob@example.com"],
      },
    });
  });

  it("won't review with an invalid recipient", () => {
    renderForm();
    fill();
    fireEvent.change(screen.getByLabelText("Recipients"), {
      target: { value: "alice@example.com\nnot-an-email" },
    });
    expect(screen.getByRole("alert").textContent).toContain("not-an-email");
    expect(
      (screen.getByRole("button", { name: "Review" }) as HTMLButtonElement)
        .disabled,
    ).toBe(true);
  });

  it("shows the server's error and goes back to the form", async () => {
    server.use(
      relayEndpoint.mutation("BroadcastFormMutation", () =>
        HttpResponse.json({
          data: null,
          errors: [
            {
              message:
                "the From address must be on this instance's domain (seslogin.test)",
              extensions: { code: "INTERNAL_SERVER_ERROR" },
            },
          ],
        }),
      ),
    );
    renderForm();
    fill();
    fireEvent.click(screen.getByRole("button", { name: "Review" }));
    fireEvent.click(screen.getByRole("button", { name: "Send now" }));
    await waitFor(() => expect(screen.getByRole("alert")).toBeTruthy());
    expect(screen.getByLabelText("Subject")).toBeTruthy();
  });

  it("explains when the instance has no domain to send from", () => {
    renderForm(vi.fn(), []);
    expect(screen.getByText(/no inbound address/)).toBeTruthy();
  });
});
