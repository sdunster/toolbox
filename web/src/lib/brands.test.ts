import { describe, expect, it } from "vitest";
import { brandChoices, brandLabel, hasExtraBrands } from "./brands";

const defaultBrand = {
  brandId: "default",
  name: "Samuel James Dunster",
  archived: false,
  isDefault: true,
};
const positiveRate = {
  brandId: "b1",
  name: "Positive Rate",
  archived: false,
  isDefault: false,
};
const old = { brandId: "b2", name: "Old", archived: true, isDefault: false };

describe("brands", () => {
  it("only shows brand controls once there is a second brand", () => {
    expect(hasExtraBrands([defaultBrand])).toBe(false);
    expect(hasExtraBrands(undefined)).toBe(false);
    expect(hasExtraBrands([defaultBrand, positiveRate])).toBe(true);
  });

  it("hides archived brands unless one is the current choice", () => {
    const all = [defaultBrand, positiveRate, old];
    expect(brandChoices(all).map((b) => b.brandId)).toEqual(["default", "b1"]);
    expect(brandChoices(all, "b2").map((b) => b.brandId)).toEqual([
      "default",
      "b1",
      "b2",
    ]);
  });

  it("labels a default brand with no business name yet", () => {
    expect(brandLabel({ ...defaultBrand, name: "" })).toBe("Default brand");
    expect(brandLabel(positiveRate)).toBe("Positive Rate");
  });
});
