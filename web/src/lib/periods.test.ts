import { describe, expect, it } from "vitest";
import { presetPeriods } from "./periods";

describe("presetPeriods", () => {
  it("finds the BAS quarters and financial years around a date", () => {
    const [thisQ, lastQ, thisFy, lastFy] = presetPeriods("2026-10-10");
    expect(thisQ).toMatchObject({ from: "2026-10-01", to: "2026-12-31" });
    expect(lastQ).toMatchObject({ from: "2026-07-01", to: "2026-09-30" });
    expect(thisFy).toMatchObject({ from: "2026-07-01", to: "2027-06-30" });
    expect(lastFy).toMatchObject({ from: "2025-07-01", to: "2026-06-30" });
  });

  it("wraps the previous quarter back a year in January", () => {
    const [thisQ, lastQ, thisFy] = presetPeriods("2027-02-15");
    expect(thisQ).toMatchObject({ from: "2027-01-01", to: "2027-03-31" });
    expect(lastQ).toMatchObject({ from: "2026-10-01", to: "2026-12-31" });
    expect(thisFy).toMatchObject({ from: "2026-07-01", to: "2027-06-30" });
  });
});
