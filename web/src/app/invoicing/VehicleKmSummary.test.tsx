import { Suspense } from "react";
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { setupServer } from "msw/node";
import { graphql as mswGraphql, HttpResponse } from "msw";
import { RelayEnvironmentProvider } from "react-relay";
import { getGraphQLEndpoint } from "../../lib/api";
import { createUnauthenticatedGraphQLEnvironment } from "../../lib/environments";
import { VehicleKmSummary } from "./VehicleKmSummary";

const relayEndpoint = mswGraphql.link(getGraphQLEndpoint());
const server = setupServer();

beforeAll(() => server.listen({ onUnhandledRequest: "error" }));
afterEach(() => server.resetHandlers());
afterAll(() => server.close());

function renderWithTotal(totalKm: string) {
  let received: Record<string, unknown> | undefined;
  server.use(
    relayEndpoint.query("VehicleKmSummaryQuery", ({ variables }) => {
      received = variables;
      return HttpResponse.json({
        data: {
          vehicleKmSummary: {
            financialYearLabel: "2026–27",
            totalKm,
            capKm: 5000,
            rateCentsPerKm: 91,
          },
        },
      });
    }),
  );
  render(
    <RelayEnvironmentProvider
      environment={createUnauthenticatedGraphQLEnvironment()}
    >
      <Suspense fallback="loading">
        <VehicleKmSummary
          instanceId="inst-ledger"
          financialYear={2026}
          refreshKey={0}
        />
      </Suspense>
    </RelayEnvironmentProvider>,
  );
  return () => received;
}

describe("VehicleKmSummary", () => {
  it("shows the running total against the cap", async () => {
    const received = renderWithTotal("1240.5");
    expect(await screen.findByTestId("vehicle-km-total")).toHaveTextContent(
      "1,240.5",
    );
    expect(screen.getByText(/FY 2026–27/)).toBeInTheDocument();
    expect(screen.getByText(/91c\/km/)).toBeInTheDocument();
    expect(screen.queryByText(/5,000 km limit/)).not.toBeInTheDocument();
    expect(received()).toEqual({
      instanceId: "inst-ledger",
      financialYear: 2026,
    });
  });

  it("warns as the total nears the cap", async () => {
    renderWithTotal("4600");
    expect(await screen.findByText(/Approaching/)).toBeInTheDocument();
  });

  it("warns past the cap without blocking anything", async () => {
    renderWithTotal("5012");
    expect(await screen.findByText(/Over the ATO/)).toBeInTheDocument();
  });
});
