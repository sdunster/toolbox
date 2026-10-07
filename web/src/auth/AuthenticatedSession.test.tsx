import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { render, screen, waitFor } from "@testing-library/react";
import { setupServer } from "msw/node";
import { graphql, HttpResponse } from "msw";
import { getGraphQLEndpoint } from "../lib/api";
import { setSessionToken, getSessionToken } from "../lib/sessionToken";
import AuthenticatedSession from "./AuthenticatedSession";

const relayEndpoint = graphql.link(getGraphQLEndpoint());

const server = setupServer();

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => {
  server.resetHandlers();
  localStorage.clear();
});
afterAll(() => server.close());

describe("AuthenticatedSession", () => {
  it("renders the login page when there is no token", async () => {
    render(
      <AuthenticatedSession>
        <div>authenticated content</div>
      </AuthenticatedSession>,
    );

    expect(
      await screen.findByRole("heading", { name: "Log in" }),
    ).toBeInTheDocument();
    expect(screen.queryByText("authenticated content")).not.toBeInTheDocument();
  });

  it("renders the authenticated tree when a token is present and the session query succeeds", async () => {
    setSessionToken("mtu_testid.test-token");
    server.use(
      relayEndpoint.query("CurrentUserProviderQuery", () =>
        HttpResponse.json({
          data: {
            me: {
              id: "user-1",
              email: "owner@example.com",
              name: "Olive Owner",
              enabled: true,
              isSuperuser: false,
              memberships: [],
            },
          },
        }),
      ),
    );

    render(
      <AuthenticatedSession>
        <div>authenticated content</div>
      </AuthenticatedSession>,
    );

    expect(
      await screen.findByText("authenticated content"),
    ).toBeInTheDocument();
  });

  it("clears the token and flips to the login page on a definitive 401", async () => {
    setSessionToken("mtu_testid.expired-token");
    server.use(
      relayEndpoint.query("CurrentUserProviderQuery", () =>
        HttpResponse.json({}, { status: 401 }),
      ),
    );

    render(
      <AuthenticatedSession>
        <div>authenticated content</div>
      </AuthenticatedSession>,
    );

    expect(
      await screen.findByRole("heading", { name: "Log in" }),
    ).toBeInTheDocument();
    await waitFor(() => expect(getSessionToken()).toBeNull());
    expect(
      screen.getByText("Your session has expired. Please log in again."),
    ).toBeInTheDocument();
  });
});
