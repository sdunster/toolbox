import { Suspense } from "react";
import { graphql } from "react-relay";
import { useParams } from "react-router";
import type { InstanceDetailPageQuery } from "./__generated__/InstanceDetailPageQuery.graphql";
import { useRetryableLazyLoadQuery } from "../../components/useRetryableLazyLoadQuery";
import RelayErrorBoundary from "../../components/RelayErrorBoundary";
import LoadingIndicator from "../../components/LoadingIndicator";
import { ButtonLink } from "../../components/ui/Button";
import { InstanceEditForm } from "./InstanceEditForm";
import { InstanceDeleteControl } from "./InstanceDeleteControl";
import { MembersSection } from "./MembersSection";
import { InboundAddressesSection } from "./InboundAddressesSection";
import { ApiTokensSection } from "./ApiTokensSection";
import { InvoicingSettingsForm } from "../invoicing/InvoicingSettingsForm";
import { InvoiceNumberingForm } from "../invoicing/InvoiceNumberingForm";
import { BrandsSection } from "../invoicing/BrandsSection";

const instanceDetailPageQuery = graphql`
  query InstanceDetailPageQuery($id: ID!) @throwOnFieldError {
    adminInstance(id: $id) {
      id
      kind
      ...InstanceEditForm_instance
      ...InvoicingSettingsForm_instance
      ...InvoiceNumberingForm_instance
      ...BrandsSection_instance
      ...InstanceDeleteControl_instance
      ...MembersSection_instance
      ...InboundAddressesSection_instance
      ...ApiTokensSection_instance
    }
    ...MembersSection_query
  }
`;

function Content({ id }: { id: string }) {
  const data = useRetryableLazyLoadQuery<InstanceDetailPageQuery>(
    instanceDetailPageQuery,
    { id },
  );

  if (!data.adminInstance) {
    return (
      <div className="rounded-lg border border-dashed border-line p-10 text-center text-ink-muted">
        <p className="font-medium text-ink">Instance not found</p>
        <ButtonLink
          to="/app/admin/instances"
          variant="secondary"
          className="mt-4"
        >
          Back to instances
        </ButtonLink>
      </div>
    );
  }

  // Inbound addresses and API tokens are support-only (the API rejects both
  // for an invoicing instance); an invoicing instance gets its business
  // settings here instead — the same form its owner sees.
  const isInvoicing = data.adminInstance.kind === "INVOICING";

  return (
    <div className="flex max-w-2xl flex-col gap-6">
      <ButtonLink
        to="/app/admin/instances"
        variant="ghost"
        className="self-start px-0"
      >
        ← Back to instances
      </ButtonLink>
      <InstanceEditForm instance={data.adminInstance} />
      {isInvoicing && (
        <>
          <InvoicingSettingsForm instance={data.adminInstance} />
          <BrandsSection instance={data.adminInstance} />
          <InvoiceNumberingForm instance={data.adminInstance} />
        </>
      )}
      <MembersSection instance={data.adminInstance} query={data} />
      {!isInvoicing && (
        <>
          <InboundAddressesSection instance={data.adminInstance} />
          <ApiTokensSection instance={data.adminInstance} />
        </>
      )}
      <InstanceDeleteControl instance={data.adminInstance} />
    </div>
  );
}

/**
 * `/app/admin/instances/:id` — edit, members, inbound addresses and API
 * tokens (support) or business settings (invoicing), and delete/restore for
 * one instance. Unlike `/app/tickets/:id`, reaching a
 * *superuser* here grants no ticket access — this page only ever touches
 * `Instance`'s own fields plus `members`/`inboundAddresses`, never
 * `tickets`/`ticket` (`Instance` has no such field at all — see
 * `adminInstances`'s doc comment in the schema).
 */
export function InstanceDetailPage() {
  const { id } = useParams<{ id: string }>();
  if (!id) return null;

  return (
    <RelayErrorBoundary canRetry>
      <Suspense fallback={<LoadingIndicator />}>
        <Content id={id} />
      </Suspense>
    </RelayErrorBoundary>
  );
}
