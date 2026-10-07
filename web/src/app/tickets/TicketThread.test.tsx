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
import { MemoryRouter } from "react-router";
import { getGraphQLEndpoint } from "../../lib/api";
import { createUnauthenticatedGraphQLEnvironment } from "../../lib/environments";
import { CurrentUserContext } from "../../auth/CurrentUserContext";
import { TicketThread } from "./TicketThread";
import type { TicketThreadHarnessQuery } from "./__generated__/TicketThreadHarnessQuery.graphql";

const fakeCurrentUser = {
  id: "user-agent",
  email: "agent@example.com",
  name: "Adrian Agent",
  enabled: true,
} as never;

const relayEndpoint = mswGraphql.link(getGraphQLEndpoint());
const server = setupServer();

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

const harnessQuery = graphql`
  query TicketThreadHarnessQuery($id: ID!) @throwOnFieldError {
    ticket(id: $id) {
      ...TicketThread_ticket
    }
  }
`;

function baseTicket() {
  return {
    __typename: "Ticket",
    id: "ticket-1",
    number: 42,
    subject: "Printer is on fire",
    createdAt: Math.floor(Date.now() / 1000) - 7200,
    status: "OPEN",
    assigneeUserId: null,
    assignee: null,
    requesterEmails: ["customer@example.com"],
    ccEmails: [],
    instance: {
      __typename: "Instance",
      id: "inst-1",
      members: [
        {
          user: {
            __typename: "User",
            id: "user-agent",
            name: "Adrian Agent",
            email: "agent@example.com",
          },
        },
        {
          user: {
            __typename: "User",
            id: "user-colleague",
            name: "Casey Colleague",
            email: "casey@example.com",
          },
        },
      ],
    },
    messages: [
      {
        __typename: "TicketMessage",
        id: "msg-1",
        kind: "INBOUND",
        fromEmail: "customer@example.com",
        author: null,
        toEmails: ["support@example.com"],
        ccEmails: [],
        bodyText: "It's actually on fire, please help.",
        createdAt: Math.floor(Date.now() / 1000) - 7200,
        attachments: [],
      },
      {
        __typename: "TicketMessage",
        id: "msg-2",
        kind: "NOTE",
        fromEmail: null,
        author: {
          __typename: "User",
          id: "user-agent",
          name: "Adrian Agent",
          email: "agent@example.com",
        },
        toEmails: [],
        ccEmails: [],
        bodyText: "Definitely do not reply-all with the fire department joke.",
        createdAt: Math.floor(Date.now() / 1000) - 3600,
        attachments: [],
      },
    ],
  };
}

function Harness({ id }: { id: string }) {
  const data = useLazyLoadQuery<TicketThreadHarnessQuery>(harnessQuery, {
    id,
  });
  if (!data.ticket) return <p>not found</p>;
  return <TicketThread ticket={data.ticket} />;
}

function renderHarness() {
  const environment = createUnauthenticatedGraphQLEnvironment();
  return render(
    <MemoryRouter>
      <CurrentUserContext value={fakeCurrentUser}>
        <RelayEnvironmentProvider environment={environment}>
          <Suspense fallback="loading">
            <Harness id="ticket-1" />
          </Suspense>
        </RelayEnvironmentProvider>
      </CurrentUserContext>
    </MemoryRouter>,
  );
}

describe("TicketThread — message kinds", () => {
  it("renders an internal note distinctly from a customer-facing message", async () => {
    server.use(
      relayEndpoint.query("TicketThreadHarnessQuery", () =>
        HttpResponse.json({ data: { ticket: baseTicket() } }),
      ),
    );

    renderHarness();

    // The note is unmistakably labelled — not just styled — as internal.
    expect(
      await screen.findByText("Internal note — not sent to the customer"),
    ).toBeInTheDocument();
    expect(
      screen.getByText(
        "Definitely do not reply-all with the fire department joke.",
      ),
    ).toBeInTheDocument();

    // The customer's own inbound message carries no such label.
    expect(
      screen.getByText("It's actually on fire, please help."),
    ).toBeInTheDocument();
    expect(
      screen.queryAllByText("Internal note — not sent to the customer"),
    ).toHaveLength(1);
  });
});

describe("TicketThread — reply mutation", () => {
  it("appends a reply to the thread without a full query refetch", async () => {
    let harnessCalls = 0;
    server.use(
      relayEndpoint.query("TicketThreadHarnessQuery", () => {
        harnessCalls += 1;
        return HttpResponse.json({ data: { ticket: baseTicket() } });
      }),
      relayEndpoint.mutation("ReplyFormMutation", ({ variables }) =>
        HttpResponse.json({
          data: {
            replyToTicket: {
              __typename: "TicketMessage",
              id: "msg-3",
              kind: "REPLY",
              fromEmail: null,
              author: {
                __typename: "User",
                id: "user-agent",
                name: "Adrian Agent",
                email: "agent@example.com",
              },
              toEmails: ["customer@example.com"],
              ccEmails: [],
              bodyText: variables.body,
              createdAt: Math.floor(Date.now() / 1000),
              attachments: [],
            },
          },
        }),
      ),
    );

    const user = UserEvent.setup();
    renderHarness();

    await screen.findByText("It's actually on fire, please help.");
    await waitFor(() => expect(harnessCalls).toBe(1));

    await user.type(
      screen.getByLabelText("Reply to customer"),
      "We've dispatched someone.",
    );
    await user.click(screen.getByRole("button", { name: "Send reply" }));

    expect(
      await screen.findByText("We've dispatched someone."),
    ).toBeInTheDocument();
    // Still just the one initial fetch — the reply landed via the
    // mutation's own updater (appending onto `ticket.messages`), not a
    // second round trip through the ticket query.
    expect(harnessCalls).toBe(1);
  });
});

describe("TicketThread — assignment", () => {
  it("lets an agent hand the ticket to a colleague", async () => {
    let assignedTo: unknown = undefined;
    server.use(
      relayEndpoint.query("TicketThreadHarnessQuery", () =>
        HttpResponse.json({ data: { ticket: baseTicket() } }),
      ),
      relayEndpoint.mutation("AssigneeControlMutation", ({ variables }) => {
        assignedTo = variables.userId;
        return HttpResponse.json({
          data: {
            assignTicket: {
              __typename: "Ticket",
              id: "ticket-1",
              assigneeUserId: "user-colleague",
              assignee: {
                __typename: "User",
                id: "user-colleague",
                name: "Casey Colleague",
                email: "casey@example.com",
              },
            },
          },
        });
      }),
    );

    const user = UserEvent.setup();
    renderHarness();

    const picker = await screen.findByLabelText("Assignee");
    await user.selectOptions(picker, "user-colleague");

    await waitFor(() => expect(assignedTo).toBe("user-colleague"));
    expect(picker).toHaveValue("user-colleague");
  });
});
