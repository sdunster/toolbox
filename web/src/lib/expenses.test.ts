import { describe, expect, it } from "vitest";
import {
  EXPENSE_CATEGORIES,
  categoryLabel,
  defaultGstCents,
  financialYearOf,
  formatFinancialYear,
  kmCapStatus,
  parseDistanceToTenths,
  rateForDate,
  tripAmountCents,
} from "./expenses";

describe("categories", () => {
  it("lists all 19 and labels them", () => {
    expect(EXPENSE_CATEGORIES).toHaveLength(19);
    expect(categoryLabel("VEHICLE_KM")).toBe("Vehicle trip (cents per km)");
    expect(categoryLabel("UNKNOWN")).toBe("UNKNOWN");
  });
});

describe("defaultGstCents", () => {
  it("is a eleventh of the GST-inclusive amount, to the cent", () => {
    expect(defaultGstCents(11_000)).toBe(1_000);
    expect(defaultGstCents(1_000)).toBe(91); // 90.9c
    expect(defaultGstCents(0)).toBe(0);
  });
});

describe("financial years", () => {
  it("turns over on 1 July", () => {
    expect(financialYearOf("2026-06-30")).toBe(2025);
    expect(financialYearOf("2026-07-01")).toBe(2026);
    expect(financialYearOf("nope")).toBeNull();
  });
  it("formats as the ATO does", () => {
    expect(formatFinancialYear(2026)).toBe("2026–27");
    expect(formatFinancialYear(2099)).toBe("2099–00");
  });
  it("looks up the rate by the date's financial year", () => {
    expect(rateForDate("2026-06-30")).toBe(88);
    expect(rateForDate("2026-07-01")).toBe(91);
    expect(rateForDate("2030-01-01")).toBeNull();
  });
});

describe("parseDistanceToTenths", () => {
  it("accepts up to one decimal place", () => {
    expect(parseDistanceToTenths("12")).toBe(120);
    expect(parseDistanceToTenths(" 12.5 ")).toBe(125);
    expect(parseDistanceToTenths(".5")).toBe(5);
    expect(parseDistanceToTenths("5000")).toBe(50_000);
  });
  it("rejects anything the API would", () => {
    for (const bad of ["", "0", "1.25", "-1", "abc", "5000.1", "1."]) {
      expect(parseDistanceToTenths(bad)).toBeNull();
    }
  });
});

describe("tripAmountCents", () => {
  it("rounds half up", () => {
    expect(tripAmountCents(125, 88)).toBe(1_100);
    expect(tripAmountCents(5, 91)).toBe(46);
    expect(tripAmountCents(1, 84)).toBe(8);
  });
});

describe("kmCapStatus", () => {
  it("is amber past 4,500 km and red past 5,000", () => {
    expect(kmCapStatus(4_500)).toBe("ok");
    expect(kmCapStatus(4_500.1)).toBe("near");
    expect(kmCapStatus(5_000)).toBe("near");
    expect(kmCapStatus(5_000.1)).toBe("over");
  });
});
