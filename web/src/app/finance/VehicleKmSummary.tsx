import { graphql } from "react-relay";
import type { VehicleKmSummaryQuery } from "./__generated__/VehicleKmSummaryQuery.graphql";
import { useRetryableLazyLoadQuery } from "../../components/useRetryableLazyLoadQuery";
import { kmCapStatus } from "../../lib/expenses";

const vehicleKmSummaryQuery = graphql`
  query VehicleKmSummaryQuery($instanceId: ID!, $financialYear: Int!)
  @throwOnFieldError {
    vehicleKmSummary(instanceId: $instanceId, financialYear: $financialYear) {
      financialYearLabel
      totalKm
      capKm
      rateCentsPerKm
    }
  }
`;

const kmFormatter = new Intl.NumberFormat("en-AU", {
  maximumFractionDigits: 1,
});

const barColor = {
  ok: "bg-accent",
  near: "bg-amber-500",
  over: "bg-red-600",
} as const;

/**
 * The caller's own cents-per-km trips this financial year against the
 * ATO's 5,000 km cap — a running total, amber past 4,500 km and red past
 * 5,000. Informational: the API never refuses a trip past the cap.
 * `refreshKey` refetches after the page adds or edits an expense.
 */
export function VehicleKmSummary({
  instanceId,
  financialYear,
  refreshKey,
}: {
  instanceId: string;
  financialYear: number;
  refreshKey: number;
}) {
  const data = useRetryableLazyLoadQuery<VehicleKmSummaryQuery>(
    vehicleKmSummaryQuery,
    { instanceId, financialYear },
    { fetchKey: refreshKey, fetchPolicy: "store-and-network" },
  );
  const summary = data.vehicleKmSummary;
  const total = Number(summary.totalKm);
  const status = kmCapStatus(total);
  const percent = Math.min(100, (total / summary.capKm) * 100);

  return (
    <div className="flex flex-col gap-2 rounded-lg border border-line p-4">
      <div className="flex flex-wrap items-baseline justify-between gap-2">
        <p className="text-sm font-medium text-ink-strong">
          Your vehicle km, FY {summary.financialYearLabel}
        </p>
        <p className="text-sm text-ink tabular-nums">
          <span data-testid="vehicle-km-total">
            {kmFormatter.format(total)}
          </span>{" "}
          / {kmFormatter.format(summary.capKm)} km
          {summary.rateCentsPerKm !== null &&
            summary.rateCentsPerKm !== undefined && (
              <span className="text-ink-muted">
                {" "}
                · {summary.rateCentsPerKm}c/km
              </span>
            )}
        </p>
      </div>
      <div
        className="h-2 overflow-hidden rounded-full bg-surface-sunken"
        role="progressbar"
        aria-label="Vehicle km against the 5,000 km cap"
        aria-valuemin={0}
        aria-valuemax={summary.capKm}
        aria-valuenow={total}
      >
        <div
          className={`h-full ${barColor[status]}`}
          style={{ width: `${percent}%` }}
        />
      </div>
      {status !== "ok" && (
        <p
          className={
            status === "over"
              ? "text-sm text-red-700 dark:text-red-400"
              : "text-sm text-amber-700 dark:text-amber-400"
          }
        >
          {status === "over"
            ? "Over the ATO's 5,000 km limit for the cents-per-km method — talk to your accountant about the logbook method."
            : "Approaching the ATO's 5,000 km limit for the cents-per-km method."}
        </p>
      )}
    </div>
  );
}
