/** The default brand's id — the instance's own invoicing settings. Mirrors
 * `db::DEFAULT_BRAND_ID`; the API takes it wherever a `brandId` goes. */
export const DEFAULT_BRAND_ID = "default";

/** What the brand pickers and filters need of `Instance.brands`. */
export interface BrandOption {
  readonly brandId: string;
  readonly name: string;
  readonly archived: boolean;
  readonly isDefault: boolean;
}

/** Whether brand controls are worth showing: only once the instance has a
 * brand besides the default one. */
export function hasExtraBrands(
  brands: ReadonlyArray<BrandOption> | null | undefined,
): boolean {
  return (brands ?? []).some((b) => !b.isDefault);
}

/** The brands a project or expense can be put under: every active one,
 * plus `current` even if it has since been archived. */
export function brandChoices(
  brands: ReadonlyArray<BrandOption>,
  current?: string | null,
): BrandOption[] {
  return brands.filter((b) => !b.archived || b.brandId === current);
}

/** A brand's display name — the default brand may have no business name
 * set yet. */
export function brandLabel(brand: BrandOption): string {
  if (brand.name) return brand.name;
  return brand.isDefault ? "Default brand" : "Unnamed brand";
}
