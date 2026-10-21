import { Suspense } from "react";
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import UserEvent from "@testing-library/user-event";
import { setupServer } from "msw/node";
import { graphql as mswGraphql, HttpResponse } from "msw";
import {
  graphql,
  useLazyLoadQuery,
  RelayEnvironmentProvider,
} from "react-relay";
import { MemoryRouter } from "react-router";
import { getGraphQLEndpoint } from "../lib/api";
import { createUnauthenticatedGraphQLEnvironment } from "../lib/environments";
import { NotificationSettingsSection } from "./NotificationSettingsSection";
import type { NotificationSettingsSectionHarnessQuery } from "./__generated__/NotificationSettingsSectionHarnessQuery.graphql";

const relayEndpoint = mswGraphql.link(getGraphQLEndpoint());
const server = setupServer();

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

const harnessQuery = graphql`
  query NotificationSettingsSectionHarnessQuery @throwOnFieldError {
    me {
      ...NotificationSettingsSection_user
    }
  }
`;

function Harness() {
  const data = useLazyLoadQuery<NotificationSettingsSectionHarnessQuery>(
    harnessQuery,
    {},
  );
  return <NotificationSettingsSection user={data.me} />;
}

function renderHarness() {
  const environment = createUnauthenticatedGraphQLEnvironment();
  return render(
    <MemoryRouter>
      <RelayEnvironmentProvider environment={environment}>
        <Suspense fallback="loading">
          <Harness />
        </Suspense>
      </RelayEnvironmentProvider>
    </MemoryRouter>,
  );
}

const oneMembership = {
  __typename: "User",
  memberships: [
    {
      instance: {
        __typename: "Instance",
        id: "inst-1",
        name: "Acme",
        kind: "SUPPORT",
      },
      notificationSettings: {
        newTicket: true,
        assignedToMe: true,
        assignedToMeUpdated: true,
        unassignedUpdated: true,
        assignedToOthersUpdated: false,
      },
    },
  ],
};

describe("NotificationSettingsSection", () => {
  it("renders one checkbox per setting, checked per the current settings", async () => {
    server.use(
      relayEndpoint.query("NotificationSettingsSectionHarnessQuery", () =>
        HttpResponse.json({ data: { me: oneMembership } }),
      ),
    );

    renderHarness();

    await screen.findByText("Acme");
    expect(
      screen.getByRole("checkbox", { name: "A new ticket is created" }),
    ).toBeChecked();
    expect(
      screen.getByRole("checkbox", {
        name: "A ticket assigned to someone else is updated",
      }),
    ).not.toBeChecked();
  });

  it("commits a single-field patch and reflects the mutation's response", async () => {
    let receivedVariables: Record<string, unknown> | undefined;
    server.use(
      relayEndpoint.query("NotificationSettingsSectionHarnessQuery", () =>
        HttpResponse.json({ data: { me: oneMembership } }),
      ),
      relayEndpoint.mutation(
        "NotificationSettingsSectionUpdateMutation",
        ({ variables }) => {
          receivedVariables = variables;
          return HttpResponse.json({
            data: {
              updateNotificationSettings: {
                notificationSettings: {
                  ...oneMembership.memberships[0].notificationSettings,
                  assignedToOthersUpdated: true,
                },
              },
            },
          });
        },
      ),
    );

    const user = UserEvent.setup();
    renderHarness();

    const checkbox = await screen.findByRole("checkbox", {
      name: "A ticket assigned to someone else is updated",
    });
    expect(checkbox).not.toBeChecked();

    await user.click(checkbox);

    expect(
      await screen.findByRole("checkbox", {
        name: "A ticket assigned to someone else is updated",
      }),
    ).toBeChecked();

    expect(receivedVariables).toMatchObject({
      instanceId: "inst-1",
      settings: { assignedToOthersUpdated: true },
    });
  });

  it("shows an error and leaves the checkbox unchanged on a failed update", async () => {
    server.use(
      relayEndpoint.query("NotificationSettingsSectionHarnessQuery", () =>
        HttpResponse.json({ data: { me: oneMembership } }),
      ),
      relayEndpoint.mutation("NotificationSettingsSectionUpdateMutation", () =>
        HttpResponse.json({
          data: null,
          errors: [{ message: "boom" }],
        }),
      ),
    );

    const user = UserEvent.setup();
    renderHarness();

    const checkbox = await screen.findByRole("checkbox", {
      name: "A ticket assigned to someone else is updated",
    });
    await user.click(checkbox);

    // `relayMutationErrorMessage` prefers the server's own message text —
    // see `lib/relayMutationError.ts` — so the fallback string never shows
    // when the mutation actually returned one.
    expect(await screen.findByText("boom")).toBeInTheDocument();
  });

  it("leaves out finance instances", async () => {
    const [support] = oneMembership.memberships;
    const finance = {
      ...support,
      instance: {
        __typename: "Instance",
        id: "inst-2",
        name: "Ledger",
        kind: "FINANCE",
      },
    };
    server.use(
      relayEndpoint.query("NotificationSettingsSectionHarnessQuery", () =>
        HttpResponse.json({
          data: {
            me: { ...oneMembership, memberships: [support, finance] },
          },
        }),
      ),
    );

    renderHarness();

    await screen.findByText("Acme");
    expect(screen.queryByText("Ledger")).not.toBeInTheDocument();
    expect(
      screen.getAllByRole("checkbox", { name: "A new ticket is created" }),
    ).toHaveLength(1);
  });

  it("explains itself when every membership is a finance one", async () => {
    const [support] = oneMembership.memberships;
    server.use(
      relayEndpoint.query("NotificationSettingsSectionHarnessQuery", () =>
        HttpResponse.json({
          data: {
            me: {
              ...oneMembership,
              memberships: [
                {
                  ...support,
                  instance: { ...support.instance, kind: "FINANCE" },
                },
              ],
            },
          },
        }),
      ),
    );

    renderHarness();

    expect(
      await screen.findByText(/only apply to support instances/),
    ).toBeInTheDocument();
    expect(screen.queryByRole("checkbox")).not.toBeInTheDocument();
  });
});
