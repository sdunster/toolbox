import { Suspense } from "react";
import { graphql } from "react-relay";
import type { FinanceSettingsPageQuery } from "./__generated__/FinanceSettingsPageQuery.graphql";
import { useRetryableLazyLoadQuery } from "../../components/useRetryableLazyLoadQuery";
import RelayErrorBoundary from "../../components/RelayErrorBoundary";
import LoadingIndicator from "../../components/LoadingIndicator";
import { RequireFinanceInstance } from "./RequireFinanceInstance";
import { FinanceSettingsForm } from "./FinanceSettingsForm";
import { InvoiceNumberingForm } from "./InvoiceNumberingForm";
import { BrandsSection } from "./BrandsSection";

const financeSettingsPageQuery = graphql`
  query FinanceSettingsPageQuery($slug: String!) @throwOnFieldError {
    instance(slug: $slug) {
      id
      ...FinanceSettingsForm_instance
      ...InvoiceNumberingForm_instance
      ...BrandsSection_instance
    }
  }
`;

function Content({ slug }: { slug: string }) {
  const data = useRetryableLazyLoadQuery<FinanceSettingsPageQuery>(
    financeSettingsPageQuery,
    { slug },
  );

  if (!data.instance) {
    return (
      <div className="rounded-lg border border-dashed border-line p-10 text-center text-ink-muted">
        Instance not found.
      </div>
    );
  }

  return (
    <div className="flex flex-col gap-6">
      <FinanceSettingsForm instance={data.instance} />
      <BrandsSection instance={data.instance} />
      <InvoiceNumberingForm instance={data.instance} />
    </div>
  );
}

/**
 * `/app/finance-settings` — the selected finance instance's business
 * details and payment footer. Owner-only to change (the nav link is hidden
 * from agents, and `updateFinanceSettings` is owner-or-superuser in the
 * API); an agent who follows a link here gets an explanation rather than a
 * form whose Save would only fail.
 */
export function FinanceSettingsPage() {
  return (
    <RequireFinanceInstance purpose="edit its business settings">
      {(instance) => (
        <div className="flex max-w-2xl flex-col gap-6">
          <div>
            <h1 className="text-xl font-semibold text-ink-strong">
              Business settings
            </h1>
            <p className="mt-1 text-sm text-ink-muted">
              Printed on every invoice {instance.name} issues.
            </p>
          </div>
          {instance.role === "OWNER" ? (
            <RelayErrorBoundary canRetry>
              <Suspense fallback={<LoadingIndicator />}>
                <Content key={instance.id} slug={instance.slug} />
              </Suspense>
            </RelayErrorBoundary>
          ) : (
            <div className="rounded-lg border border-dashed border-line p-10 text-center text-ink-muted">
              Only an owner of {instance.name} can change its business settings.
            </div>
          )}
        </div>
      )}
    </RequireFinanceInstance>
  );
}
