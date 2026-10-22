import { describe, expect, it, vi } from "vitest";
import { render, screen } from "@testing-library/react";
import UserEvent from "@testing-library/user-event";
import { ExpenseForm } from "./ExpenseForm";
import { blankExpense } from "./expenseFormDefaults";

function renderForm(
  onSubmit = vi.fn(),
  initial = blankExpense("2026-07-15", null),
) {
  render(
    <ExpenseForm
      idPrefix="t"
      currency="AUD"
      initial={initial}
      projects={[{ id: "p1", name: "Fictional Job" }]}
      submitLabel="Add expense"
      savingLabel="Adding…"
      isSaving={false}
      error={null}
      onSubmit={onSubmit}
    />,
  );
  return onSubmit;
}

describe("ExpenseForm", () => {
  it("pre-fills GST as a eleventh of the amount until overridden", async () => {
    const user = UserEvent.setup();
    const onSubmit = renderForm();
    await user.type(screen.getByLabelText("Supplier"), "Fictional Hardware");
    await user.type(screen.getByLabelText("Amount incl. GST (AUD)"), "110");
    expect(screen.getByLabelText("GST (AUD)")).toHaveValue("10");

    await user.click(screen.getByRole("button", { name: "Add expense" }));
    expect(onSubmit).toHaveBeenLastCalledWith({
      projectId: null,
      brandId: "default",
      date: "2026-07-15",
      category: "MATERIALS",
      description: null,
      supplier: "Fictional Hardware",
      amountCents: 11_000,
      gstCents: 1_000,
      businessUsePercent: 100,
      gstNotClaimable: false,
      distanceKm: null,
    });

    await user.clear(screen.getByLabelText("GST (AUD)"));
    await user.type(screen.getByLabelText("GST (AUD)"), "4.50");
    await user.type(screen.getByLabelText("Amount incl. GST (AUD)"), "0");
    // A typed GST stays put when the amount changes.
    expect(screen.getByLabelText("GST (AUD)")).toHaveValue("4.50");
    await user.selectOptions(screen.getByLabelText("Project"), "p1");
    await user.click(screen.getByRole("button", { name: "Add expense" }));
    expect(onSubmit).toHaveBeenLastCalledWith(
      expect.objectContaining({
        projectId: "p1",
        amountCents: 110_000,
        gstCents: 450,
      }),
    );
  });

  it("sends no GST for a GST-free purchase", async () => {
    const user = UserEvent.setup();
    const onSubmit = renderForm();
    await user.type(screen.getByLabelText("Supplier"), "Fictional Bank");
    await user.type(screen.getByLabelText("Amount incl. GST (AUD)"), "12");
    await user.click(screen.getByLabelText("GST-free"));
    expect(screen.getByLabelText("GST (AUD)")).toBeDisabled();
    await user.click(screen.getByRole("button", { name: "Add expense" }));
    expect(onSubmit).toHaveBeenLastCalledWith(
      expect.objectContaining({ amountCents: 1_200, gstCents: null }),
    );
  });

  it("refuses GST larger than the amount", async () => {
    const user = UserEvent.setup();
    const onSubmit = renderForm();
    await user.type(screen.getByLabelText("Supplier"), "Fictional Hardware");
    await user.type(screen.getByLabelText("Amount incl. GST (AUD)"), "10");
    await user.clear(screen.getByLabelText("GST (AUD)"));
    await user.type(screen.getByLabelText("GST (AUD)"), "20");
    await user.click(screen.getByRole("button", { name: "Add expense" }));
    expect(onSubmit).not.toHaveBeenCalled();
    expect(screen.getByRole("alert")).toHaveTextContent("GST included");
  });

  it("swaps to distance and purpose for a vehicle trip, with a km × rate preview", async () => {
    const user = UserEvent.setup();
    const onSubmit = renderForm();
    await user.selectOptions(screen.getByLabelText("Category"), "VEHICLE_KM");
    expect(screen.queryByLabelText("Supplier")).not.toBeInTheDocument();
    await user.type(screen.getByLabelText("Business purpose"), "Site visit");
    await user.type(screen.getByLabelText("Distance (km)"), "12.5");
    // 15 July 2026 is FY 2026–27: 91c/km → 12.5 × 91 = 1137.5c → $11.38.
    expect(screen.getByTestId("t-trip-amount")).toHaveTextContent("11.38 AUD");
    expect(screen.getByText(/91c\/km \(FY 2026–27\)/)).toBeInTheDocument();

    await user.click(screen.getByRole("button", { name: "Add expense" }));
    expect(onSubmit).toHaveBeenLastCalledWith({
      projectId: null,
      brandId: "default",
      date: "2026-07-15",
      category: "VEHICLE_KM",
      description: "Site visit",
      supplier: null,
      amountCents: null,
      gstCents: null,
      businessUsePercent: null,
      gstNotClaimable: false,
      distanceKm: "12.5",
    });
  });

  it("sends business use and claimability, ticking it for entertainment", async () => {
    const user = UserEvent.setup();
    const onSubmit = renderForm();
    await user.type(screen.getByLabelText("Supplier"), "Fictional Telco");
    await user.type(screen.getByLabelText("Amount incl. GST (AUD)"), "110");
    await user.type(screen.getByLabelText("Business use (%)"), "60");
    expect(
      screen.getByText(/claims 6\.00 AUD of GST \(60% business use\)/),
    ).toBeInTheDocument();
    await user.click(screen.getByRole("button", { name: "Add expense" }));
    expect(onSubmit).toHaveBeenLastCalledWith(
      expect.objectContaining({
        businessUsePercent: 60,
        gstNotClaimable: false,
      }),
    );

    await user.selectOptions(
      screen.getByLabelText("Category"),
      "MEALS_ENTERTAINMENT",
    );
    expect(screen.getByLabelText("GST can't be claimed")).toBeChecked();
    await user.click(screen.getByRole("button", { name: "Add expense" }));
    expect(onSubmit).toHaveBeenLastCalledWith(
      expect.objectContaining({ gstNotClaimable: true }),
    );
  });

  it("shows a brand picker only with a second brand, following the project", async () => {
    const user = UserEvent.setup();
    const onSubmit = vi.fn();
    const brands = [
      {
        brandId: "default",
        name: "Legal Name",
        archived: false,
        isDefault: true,
      },
      {
        brandId: "b1",
        name: "Trading Name",
        archived: false,
        isDefault: false,
      },
    ];
    render(
      <ExpenseForm
        idPrefix="t"
        currency="AUD"
        initial={blankExpense("2026-07-15", null)}
        projects={[
          { id: "p1", name: "Fictional Job", brand: { brandId: "b1" } },
          { id: "p2", name: "Other Job", brand: { brandId: "default" } },
        ]}
        brands={brands}
        submitLabel="Add expense"
        savingLabel="Adding…"
        isSaving={false}
        error={null}
        onSubmit={onSubmit}
      />,
    );
    expect(screen.getByLabelText("Brand")).toHaveValue("default");
    await user.selectOptions(screen.getByLabelText("Project"), "p1");
    expect(screen.getByLabelText("Brand")).toHaveValue("b1");
    await user.selectOptions(screen.getByLabelText("Project"), "p2");
    expect(screen.getByLabelText("Brand")).toHaveValue("default");
    await user.selectOptions(screen.getByLabelText("Brand"), "b1");
    await user.type(screen.getByLabelText("Supplier"), "Fictional Hardware");
    await user.type(screen.getByLabelText("Amount incl. GST (AUD)"), "110");
    await user.click(screen.getByRole("button", { name: "Add expense" }));
    expect(onSubmit).toHaveBeenLastCalledWith(
      expect.objectContaining({ projectId: "p2", brandId: "b1" }),
    );
  });

  it("has no brand picker while the instance has one brand", () => {
    renderForm();
    expect(screen.queryByLabelText("Brand")).not.toBeInTheDocument();
  });
});
