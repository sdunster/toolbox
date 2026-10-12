import { useState } from "react";
import { graphql, useFragment, useMutation } from "react-relay";
import type { InvoicingSettingsForm_instance$key } from "./__generated__/InvoicingSettingsForm_instance.graphql";
import type { InvoicingSettingsFormMutation } from "./__generated__/InvoicingSettingsFormMutation.graphql";
import { relayMutationErrorMessage } from "../../lib/relayMutationError";
import { Card } from "../../components/ui/Card";
import { Button } from "../../components/ui/Button";
import { FormField } from "../../components/ui/FormField";
import TextInput from "../../components/ui/TextInput";
import { inputBase } from "../../components/ui/inputStyles";

const invoicingSettingsFormFragment = graphql`
  fragment InvoicingSettingsForm_instance on Instance {
    id
    invoicingSettings {
      businessName
      businessAbn
      businessAddress
      businessPhone
      businessEmail
      paymentDetails
      gstRegistered
      currency
      paymentTermsDays
    }
  }
`;

/**
 * An invoicing instance's seller details and payment footer — what every
 * invoice prints about the business issuing it. Shared by the owner's
 * `/app/invoicing-settings` page and the superuser admin instance page.
 *
 * `updateInvoicingSettings` is a full replace: every submit sends every
 * field, and a blank one clears it server-side (a blank currency falls back
 * to AUD). Renders nothing for a support instance, whose
 * `invoicingSettings` is `null`.
 */
export function InvoicingSettingsForm({
  instance,
}: {
  instance: InvoicingSettingsForm_instance$key;
}) {
  const data = useFragment(invoicingSettingsFormFragment, instance);
  const settings = data.invoicingSettings;
  const [businessName, setBusinessName] = useState(
    settings?.businessName ?? "",
  );
  const [businessAbn, setBusinessAbn] = useState(settings?.businessAbn ?? "");
  const [businessAddress, setBusinessAddress] = useState(
    settings?.businessAddress ?? "",
  );
  const [businessPhone, setBusinessPhone] = useState(
    settings?.businessPhone ?? "",
  );
  const [businessEmail, setBusinessEmail] = useState(
    settings?.businessEmail ?? "",
  );
  const [paymentDetails, setPaymentDetails] = useState(
    settings?.paymentDetails ?? "",
  );
  const [gstRegistered, setGstRegistered] = useState(
    settings?.gstRegistered ?? false,
  );
  const [currency, setCurrency] = useState(settings?.currency ?? "AUD");
  const [paymentTermsDays, setPaymentTermsDays] = useState(
    String(settings?.paymentTermsDays ?? 14),
  );
  const [error, setError] = useState<string | null>(null);
  const [saved, setSaved] = useState(false);

  const [commit, isSaving] = useMutation<InvoicingSettingsFormMutation>(graphql`
    mutation InvoicingSettingsFormMutation(
      $instanceId: ID!
      $input: InvoicingSettingsInput!
    ) {
      updateInvoicingSettings(instanceId: $instanceId, input: $input) {
        ...InvoicingSettingsForm_instance
      }
    }
  `);

  if (!settings) return null;

  function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (isSaving) return;
    setError(null);
    setSaved(false);
    commit({
      variables: {
        instanceId: data.id,
        input: {
          businessName: businessName.trim(),
          businessAbn: businessAbn.trim(),
          businessAddress: businessAddress.trim(),
          businessPhone: businessPhone.trim(),
          businessEmail: businessEmail.trim(),
          paymentDetails: paymentDetails.trim(),
          gstRegistered,
          currency: currency.trim().toUpperCase(),
          paymentTermsDays:
            paymentTermsDays.trim() === ""
              ? null
              : Number.parseInt(paymentTermsDays, 10),
        },
      },
      onCompleted: () => setSaved(true),
      onError: (err) =>
        setError(
          relayMutationErrorMessage(err, "Failed to save business settings."),
        ),
    });
  }

  return (
    <Card>
      <h2 className="mb-4 text-sm font-semibold tracking-wide text-ink-muted uppercase">
        Business details
      </h2>
      <form onSubmit={handleSubmit} className="flex flex-col gap-4">
        <FormField label="Business name" htmlFor="invoicing-business-name">
          <TextInput
            id="invoicing-business-name"
            value={businessName}
            onChange={(e) => setBusinessName(e.target.value)}
            maxLength={200}
          />
        </FormField>
        <FormField label="ABN" htmlFor="invoicing-business-abn">
          <TextInput
            id="invoicing-business-abn"
            value={businessAbn}
            onChange={(e) => setBusinessAbn(e.target.value)}
            maxLength={200}
          />
        </FormField>
        <FormField label="Address" htmlFor="invoicing-business-address">
          <textarea
            id="invoicing-business-address"
            className={inputBase}
            rows={3}
            value={businessAddress}
            onChange={(e) => setBusinessAddress(e.target.value)}
            maxLength={2000}
          />
        </FormField>
        <div className="grid gap-4 sm:grid-cols-2">
          <FormField label="Phone" htmlFor="invoicing-business-phone">
            <TextInput
              id="invoicing-business-phone"
              type="tel"
              value={businessPhone}
              onChange={(e) => setBusinessPhone(e.target.value)}
              maxLength={200}
            />
          </FormField>
          <FormField label="Email" htmlFor="invoicing-business-email">
            <TextInput
              id="invoicing-business-email"
              type="email"
              value={businessEmail}
              onChange={(e) => setBusinessEmail(e.target.value)}
              maxLength={200}
            />
          </FormField>
        </div>
        <FormField label="Payment details" htmlFor="invoicing-payment-details">
          <textarea
            id="invoicing-payment-details"
            className={inputBase}
            rows={4}
            value={paymentDetails}
            onChange={(e) => setPaymentDetails(e.target.value)}
            maxLength={2000}
          />
          <p className="mt-1 text-xs text-ink-muted">
            Printed at the bottom of every invoice, e.g. bank account details.
          </p>
        </FormField>
        <div className="flex flex-col gap-1">
          <label className="flex items-center gap-2 text-sm text-ink">
            <input
              type="checkbox"
              checked={gstRegistered}
              onChange={(e) => setGstRegistered(e.target.checked)}
              className="size-4 rounded-sm border-line text-accent focus:ring-2 focus:ring-accent/25"
            />
            Registered for GST
          </label>
          <p className="text-xs text-ink-muted">
            When off, invoices print &quot;No GST has been charged.&quot; When
            on, they&apos;re titled &quot;Tax Invoice&quot; and add 10% GST.
          </p>
        </div>
        <FormField label="Currency" htmlFor="invoicing-currency">
          {/* `inputBase` is `w-full`; the wrapper narrows it to fit a code. */}
          <div className="w-24">
            <TextInput
              id="invoicing-currency"
              value={currency}
              onChange={(e) => setCurrency(e.target.value)}
              maxLength={3}
              pattern="[A-Za-z]{3}"
              placeholder="AUD"
              className="uppercase"
            />
          </div>
          <p className="mt-1 text-xs text-ink-muted">
            A three-letter code, used in invoice column and total headings.
            Defaults to AUD.
          </p>
        </FormField>

        <FormField label="Payment terms (days)" htmlFor="invoicing-terms">
          <div className="w-24">
            <TextInput
              id="invoicing-terms"
              type="number"
              min={0}
              max={365}
              value={paymentTermsDays}
              onChange={(e) => setPaymentTermsDays(e.target.value)}
            />
          </div>
          <p className="mt-1 text-xs text-ink-muted">
            An invoice is due this many days after it&apos;s issued, unless its
            project says otherwise. Defaults to 14.
          </p>
        </FormField>

        {error && (
          <p role="alert" className="text-sm text-red-600 dark:text-red-400">
            {error}
          </p>
        )}
        {saved && !error && (
          <p className="text-sm text-green-700 dark:text-green-400">Saved.</p>
        )}

        <Button type="submit" disabled={isSaving} className="self-start">
          {isSaving ? "Saving…" : "Save"}
        </Button>
      </form>
    </Card>
  );
}
