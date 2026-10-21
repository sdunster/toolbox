import { useState } from "react";
import { graphql, useFragment, useMutation } from "react-relay";
import type {
  BrandsSection_instance$data,
  BrandsSection_instance$key,
} from "./__generated__/BrandsSection_instance.graphql";
import type { BrandsSectionAddMutation } from "./__generated__/BrandsSectionAddMutation.graphql";
import type { BrandsSectionUpdateMutation } from "./__generated__/BrandsSectionUpdateMutation.graphql";
import { relayMutationErrorMessage } from "../../lib/relayMutationError";
import { Card } from "../../components/ui/Card";
import { Button } from "../../components/ui/Button";
import { FormField } from "../../components/ui/FormField";
import TextInput from "../../components/ui/TextInput";
import { inputBase } from "../../components/ui/inputStyles";

const brandsSectionFragment = graphql`
  fragment BrandsSection_instance on Instance {
    id
    brands {
      brandId
      name
      email
      phone
      address
      paymentDetails
      archived
      isDefault
    }
  }
`;

type BrandRow = BrandsSection_instance$data["brands"][number];

type BrandDraft = {
  name: string;
  email: string;
  phone: string;
  address: string;
  paymentDetails: string;
};

const emptyDraft: BrandDraft = {
  name: "",
  email: "",
  phone: "",
  address: "",
  paymentDetails: "",
};

function draftOf(brand: BrandRow): BrandDraft {
  return {
    name: brand.name,
    email: brand.email ?? "",
    phone: brand.phone ?? "",
    address: brand.address ?? "",
    paymentDetails: brand.paymentDetails ?? "",
  };
}

function inputOf(draft: BrandDraft) {
  return {
    name: draft.name.trim(),
    email: draft.email.trim(),
    phone: draft.phone.trim(),
    address: draft.address.trim(),
    paymentDetails: draft.paymentDetails.trim(),
  };
}

/**
 * The names this invoicing instance issues under (`Instance.brands`) —
 * owner-or-superuser to change (`addBrand`/`updateBrand`), shown on
 * `/app/invoicing-settings` beside the business details, which are the
 * default brand. A brand's blank contact/payment fields fall back to the
 * business details when an invoice is printed. There is no delete: an
 * archived brand can't be picked for a new project or expense but stays
 * in filters, since its invoices still exist.
 */
export function BrandsSection({
  instance,
}: {
  instance: BrandsSection_instance$key;
}) {
  const data = useFragment(brandsSectionFragment, instance);
  const others = data.brands.filter((b) => !b.isDefault);
  const defaultName = data.brands.find((b) => b.isDefault)?.name ?? "";
  const [editing, setEditing] = useState<string | null>(null);
  const [adding, setAdding] = useState(false);

  return (
    <Card>
      <h2 className="mb-1 text-sm font-semibold tracking-wide text-ink-muted uppercase">
        Brands
      </h2>
      <p className="mb-4 text-sm text-ink-muted">
        Other names you invoice under with the same ABN, such as a registered
        business name. The business details above are the default brand
        {defaultName ? ` (${defaultName})` : ""}. Each project picks the brand
        its invoices are issued under, and reports can be filtered by brand.
        Blank fields use the business details.
      </p>

      {others.length > 0 && (
        <ul className="mb-4 flex flex-col divide-y divide-line rounded-lg border border-line">
          {others.map((brand) =>
            editing === brand.brandId ? (
              <li key={brand.brandId} className="p-4">
                <BrandForm
                  instanceId={data.id}
                  brand={brand}
                  onDone={() => setEditing(null)}
                />
              </li>
            ) : (
              <li
                key={brand.brandId}
                className="flex items-center justify-between gap-4 p-4"
              >
                <div className="min-w-0">
                  <p className="font-medium text-ink-strong">
                    {brand.name}
                    {brand.archived && (
                      <span className="ml-2 text-xs font-normal text-ink-muted">
                        Archived
                      </span>
                    )}
                  </p>
                  {brand.email && (
                    <p className="truncate text-sm text-ink-muted">
                      {brand.email}
                    </p>
                  )}
                </div>
                <Button
                  variant="secondary"
                  onClick={() => {
                    setAdding(false);
                    setEditing(brand.brandId);
                  }}
                >
                  Edit
                </Button>
              </li>
            ),
          )}
        </ul>
      )}

      {adding ? (
        <BrandForm instanceId={data.id} onDone={() => setAdding(false)} />
      ) : (
        <Button
          variant="secondary"
          onClick={() => {
            setEditing(null);
            setAdding(true);
          }}
        >
          Add brand
        </Button>
      )}
    </Card>
  );
}

/** Add a brand (no `brand`) or edit one, including archiving it. */
function BrandForm({
  instanceId,
  brand,
  onDone,
}: {
  instanceId: string;
  brand?: BrandRow;
  onDone: () => void;
}) {
  const [draft, setDraft] = useState<BrandDraft>(
    brand ? draftOf(brand) : emptyDraft,
  );
  const [archived, setArchived] = useState(brand?.archived ?? false);
  const [error, setError] = useState<string | null>(null);
  const idPrefix = `brand-${brand?.brandId ?? "new"}`;

  const [commitAdd, isAdding] = useMutation<BrandsSectionAddMutation>(graphql`
    mutation BrandsSectionAddMutation($instanceId: ID!, $input: BrandInput!) {
      addBrand(instanceId: $instanceId, input: $input) {
        ...BrandsSection_instance
      }
    }
  `);
  const [commitUpdate, isUpdating] = useMutation<BrandsSectionUpdateMutation>(
    graphql`
      mutation BrandsSectionUpdateMutation(
        $instanceId: ID!
        $brandId: ID!
        $input: BrandInput!
        $archived: Boolean!
      ) {
        updateBrand(
          instanceId: $instanceId
          brandId: $brandId
          input: $input
          archived: $archived
        ) {
          ...BrandsSection_instance
        }
      }
    `,
  );
  const isSaving = isAdding || isUpdating;

  function set<K extends keyof BrandDraft>(key: K, value: string) {
    setDraft((d) => ({ ...d, [key]: value }));
  }

  function handleSubmit(e: React.FormEvent) {
    e.preventDefault();
    if (isSaving) return;
    if (draft.name.trim() === "") {
      setError("Enter the brand's name.");
      return;
    }
    setError(null);
    const onError = (err: Error) =>
      setError(relayMutationErrorMessage(err, "Failed to save the brand."));
    if (brand) {
      commitUpdate({
        variables: {
          instanceId,
          brandId: brand.brandId,
          input: inputOf(draft),
          archived,
        },
        onCompleted: onDone,
        onError,
      });
    } else {
      commitAdd({
        variables: { instanceId, input: inputOf(draft) },
        onCompleted: onDone,
        onError,
      });
    }
  }

  return (
    <form onSubmit={handleSubmit} className="flex flex-col gap-4">
      <FormField label="Name" htmlFor={`${idPrefix}-name`}>
        <TextInput
          id={`${idPrefix}-name`}
          value={draft.name}
          onChange={(e) => set("name", e.target.value)}
          maxLength={200}
          placeholder="e.g. a registered business name"
        />
      </FormField>
      <div className="grid gap-4 sm:grid-cols-2">
        <FormField label="Email" htmlFor={`${idPrefix}-email`}>
          <TextInput
            id={`${idPrefix}-email`}
            type="email"
            value={draft.email}
            onChange={(e) => set("email", e.target.value)}
            maxLength={200}
          />
        </FormField>
        <FormField label="Phone" htmlFor={`${idPrefix}-phone`}>
          <TextInput
            id={`${idPrefix}-phone`}
            type="tel"
            value={draft.phone}
            onChange={(e) => set("phone", e.target.value)}
            maxLength={200}
          />
        </FormField>
      </div>
      <FormField label="Address" htmlFor={`${idPrefix}-address`}>
        <textarea
          id={`${idPrefix}-address`}
          className={inputBase}
          rows={3}
          value={draft.address}
          onChange={(e) => set("address", e.target.value)}
          maxLength={2000}
        />
      </FormField>
      <FormField label="Payment details" htmlFor={`${idPrefix}-payment`}>
        <textarea
          id={`${idPrefix}-payment`}
          className={inputBase}
          rows={3}
          value={draft.paymentDetails}
          onChange={(e) => set("paymentDetails", e.target.value)}
          maxLength={2000}
        />
      </FormField>
      {brand && (
        <div className="flex flex-col gap-1">
          <label className="flex items-center gap-2 text-sm text-ink">
            <input
              type="checkbox"
              checked={archived}
              onChange={(e) => setArchived(e.target.checked)}
              className="size-4 rounded-sm border-line text-accent focus:ring-2 focus:ring-accent/25"
            />
            Archived
          </label>
          <p className="text-xs text-ink-muted">
            An archived brand can&apos;t be chosen for new projects or expenses.
            Its invoices are unaffected.
          </p>
        </div>
      )}

      {error && (
        <p role="alert" className="text-sm text-red-600 dark:text-red-400">
          {error}
        </p>
      )}

      <div className="flex gap-2">
        <Button type="submit" disabled={isSaving}>
          {isSaving ? "Saving…" : brand ? "Save" : "Add brand"}
        </Button>
        <Button type="button" variant="ghost" onClick={onDone}>
          Cancel
        </Button>
      </div>
    </form>
  );
}
