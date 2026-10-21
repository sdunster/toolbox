import { Suspense } from "react";
import { FormField } from "../../components/ui/FormField";
import { inputBase } from "../../components/ui/inputStyles";
import {
  type BrandOption,
  brandChoices,
  brandLabel,
  hasExtraBrands,
} from "../../lib/brands";
import { useInstanceBrands } from "./useInstanceBrands";

/**
 * Pick the brand a project's invoices (or an expense) go under. Renders
 * nothing while the instance has only its default brand, so an instance
 * that doesn't use brands sees no change. `current` is the record's stored
 * brand, which stays choosable once archived.
 */
export function BrandSelect({
  id,
  brands,
  value,
  current,
  onChange,
  help,
}: {
  id: string;
  brands: ReadonlyArray<BrandOption> | null | undefined;
  value: string;
  current?: string | null;
  onChange: (brandId: string) => void;
  help?: string;
}) {
  if (!brands || !hasExtraBrands(brands)) return null;
  return (
    <FormField label="Brand" htmlFor={id}>
      <select
        id={id}
        className={inputBase}
        value={value}
        onChange={(e) => onChange(e.target.value)}
      >
        {brandChoices(brands, current).map((b) => (
          <option key={b.brandId} value={b.brandId}>
            {brandLabel(b)}
            {b.archived ? " (archived)" : ""}
          </option>
        ))}
      </select>
      {help && <p className="mt-1 text-xs text-ink-muted">{help}</p>}
    </FormField>
  );
}

/**
 * "All brands" or one brand, for a list or report. Renders nothing while
 * the instance has only its default brand. Archived brands are listed too:
 * their invoices still exist.
 */
export function BrandFilterSelect({
  id,
  brands,
  value,
  onChange,
  className,
}: {
  id: string;
  brands: ReadonlyArray<BrandOption> | null | undefined;
  value: string | null;
  onChange: (brandId: string | null) => void;
  className?: string;
}) {
  if (!brands || !hasExtraBrands(brands)) return null;
  return (
    <div className={className}>
      <label htmlFor={id} className="sr-only">
        Filter by brand
      </label>
      <select
        id={id}
        className={inputBase}
        value={value ?? ""}
        onChange={(e) => onChange(e.target.value || null)}
      >
        <option value="">All brands</option>
        {brands.map((b) => (
          <option key={b.brandId} value={b.brandId}>
            {brandLabel(b)}
            {b.archived ? " (archived)" : ""}
          </option>
        ))}
      </select>
    </div>
  );
}

function LoadedBrandFilter({
  slug,
  ...props
}: { slug: string } & Omit<Parameters<typeof BrandFilterSelect>[0], "brands">) {
  const brands = useInstanceBrands(slug);
  return <BrandFilterSelect brands={brands} {...props} />;
}

/** {@link BrandFilterSelect} that loads the instance's brands itself —
 * for page headers, outside the page's own query. Shows nothing while
 * loading. */
export function InstanceBrandFilter(
  props: { slug: string } & Omit<
    Parameters<typeof BrandFilterSelect>[0],
    "brands"
  >,
) {
  return (
    <Suspense fallback={null}>
      <LoadedBrandFilter {...props} />
    </Suspense>
  );
}

function LoadedBrandSelect({
  slug,
  ...props
}: { slug: string } & Omit<Parameters<typeof BrandSelect>[0], "brands">) {
  const brands = useInstanceBrands(slug);
  return <BrandSelect brands={brands} {...props} />;
}

/** {@link BrandSelect} that loads the instance's brands itself. Shows
 * nothing while loading. */
export function InstanceBrandSelect(
  props: { slug: string } & Omit<Parameters<typeof BrandSelect>[0], "brands">,
) {
  return (
    <Suspense fallback={null}>
      <LoadedBrandSelect {...props} />
    </Suspense>
  );
}
