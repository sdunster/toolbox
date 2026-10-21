import { graphql } from "react-relay";
import type { useInstanceBrandsQuery } from "./__generated__/useInstanceBrandsQuery.graphql";
import { useRetryableLazyLoadQuery } from "../../components/useRetryableLazyLoadQuery";
import type { BrandOption } from "../../lib/brands";

// The brands are read by `BrandSelect`/`BrandFilterSelect` through the
// return value, so the lint rule can't see them used.
/* eslint-disable relay/unused-fields */
const instanceBrandsQuery = graphql`
  query useInstanceBrandsQuery($slug: String!) @throwOnFieldError {
    instance(slug: $slug) {
      id
      brands {
        brandId
        name
        archived
        isDefault
      }
    }
  }
`;
/* eslint-enable relay/unused-fields */

/**
 * The instance's brands (default first), for the brand pickers and
 * filters. Suspends on first use; `store-or-network` after that, and
 * `addBrand`/`updateBrand` return `brands` on the same instance record, so
 * a brand added on the settings page shows up here without a refetch.
 */
export function useInstanceBrands(slug: string): ReadonlyArray<BrandOption> {
  const data = useRetryableLazyLoadQuery<useInstanceBrandsQuery>(
    instanceBrandsQuery,
    { slug },
    { fetchPolicy: "store-or-network" },
  );
  return data.instance?.brands ?? [];
}
