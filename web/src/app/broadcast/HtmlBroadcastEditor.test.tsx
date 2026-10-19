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
import {
  HtmlBroadcastEditor,
  type BroadcastDraft,
} from "./HtmlBroadcastEditor";

const relayEndpoint = mswGraphql.link(getGraphQLEndpoint());
const server = setupServer();

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

const HTML = '<h1>Big news</h1><img src="images/logo.png">';
const HOSTED_HTML =
  '<h1>Big news</h1><img src="https://app.test/email-assets/i/x/logo.png">';

const DRAFT: BroadcastDraft = {
  id: "d-1",
  fromAddress: "news@seslogin.test",
  fromName: "",
  subject: "Big news",
  body: "",
  html: HOSTED_HTML,
  missingAssets: [],
  lastTestSentAt: null,
  recipients: [{ email: "alice@example.com" }, { email: "bob@example.com" }],
};

function renderEditor(draft: BroadcastDraft | null, onSent = vi.fn()) {
  render(
    <RelayEnvironmentProvider
      environment={createUnauthenticatedGraphQLEnvironment()}
    >
      <HtmlBroadcastEditor
        instanceId="inst-1"
        defaultFromName="seslogin Support"
        domains={["seslogin.test"]}
        testEmail="owner@example.com"
        draft={draft}
        onSaved={vi.fn()}
        onSent={onSent}
        onClose={vi.fn()}
      />
    </RelayEnvironmentProvider>,
  );
  return onSent;
}

describe("HtmlBroadcastEditor", () => {
  it("saves pasted HTML as a draft and asks for its missing images", async () => {
    let input: Record<string, unknown> | undefined;
    server.use(
      relayEndpoint.mutation(
        "HtmlBroadcastEditorSaveMutation",
        ({ variables }) => {
          input = variables.input as Record<string, unknown>;
          return HttpResponse.json({
            data: {
              saveBroadcastDraft: {
                id: "d-1",
                html: HTML,
                missingAssets: ["images/logo.png"],
                lastTestSentAt: null,
              },
            },
          });
        },
      ),
    );
    renderEditor(null);
    fireEvent.change(screen.getByLabelText("From address"), {
      target: { value: "news" },
    });
    fireEvent.change(screen.getByLabelText("Subject"), {
      target: { value: "Big news" },
    });
    fireEvent.change(screen.getByLabelText("HTML"), {
      target: { value: HTML },
    });
    fireEvent.click(screen.getByRole("button", { name: "Save draft" }));

    expect(
      await screen.findByLabelText("Upload images/logo.png"),
    ).toBeInTheDocument();
    expect(input).toMatchObject({
      id: null,
      fromAddress: "news@seslogin.test",
      subject: "Big news",
      html: HTML,
      body: "",
      recipients: [],
    });
    // Nothing can be tested or sent with an image missing.
    expect(screen.getByRole("button", { name: "Send test" })).toBeDisabled();
    expect(
      screen.getByRole("button", { name: "Review & send" }),
    ).toBeDisabled();
  });

  it("previews in a sandboxed frame", () => {
    renderEditor(DRAFT);
    const frame = screen.getByTitle("Email preview");
    expect(frame.getAttribute("sandbox")).toBe("");
    expect(frame.getAttribute("srcdoc")).toBe(HOSTED_HTML);
  });

  it("sends a test to the viewer, then the real thing after review", async () => {
    let testVars: Record<string, unknown> | undefined;
    let sendVars: Record<string, unknown> | undefined;
    server.use(
      relayEndpoint.mutation(
        "HtmlBroadcastEditorTestMutation",
        ({ variables }) => {
          testVars = variables;
          return HttpResponse.json({
            data: {
              sendBroadcastTest: { id: "d-1", lastTestSentAt: 1_800_000_000 },
            },
          });
        },
      ),
      relayEndpoint.mutation(
        "HtmlBroadcastEditorSendMutation",
        ({ variables }) => {
          sendVars = variables;
          return HttpResponse.json({
            data: {
              sendBroadcastDraft: {
                id: "d-1",
                pendingCount: 2,
                recipientCount: 2,
              },
            },
          });
        },
      ),
    );
    const onSent = renderEditor(DRAFT);

    fireEvent.click(screen.getByRole("button", { name: "Send test" }));
    expect(
      await screen.findByText(/Test sent to owner@example.com/),
    ).toBeVisible();
    expect(testVars).toEqual({
      instanceId: "inst-1",
      broadcastId: "d-1",
      to: ["owner@example.com"],
    });

    fireEvent.click(screen.getByRole("button", { name: "Review & send" }));
    expect(screen.getByText("Send to 2 recipients?")).toBeVisible();
    expect(
      screen.queryByText(/haven't sent yourself a test/),
    ).not.toBeInTheDocument();
    fireEvent.click(screen.getByRole("button", { name: "Send now" }));
    await waitFor(() => expect(onSent).toHaveBeenCalled());
    expect(sendVars).toEqual({ instanceId: "inst-1", broadcastId: "d-1" });
  });

  it("warns when the version being sent was never tested", () => {
    renderEditor(DRAFT);
    fireEvent.click(screen.getByRole("button", { name: "Review & send" }));
    expect(screen.getByText(/haven't sent yourself a test/)).toBeVisible();
  });
});
