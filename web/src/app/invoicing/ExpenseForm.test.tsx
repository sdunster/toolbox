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
      date: "2026-07-15",
      category: "MATERIALS",
      description: null,
      supplier: "Fictional Hardware",
      amountCents: 11_000,
      gstCents: 1_000,
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
      date: "2026-07-15",
      category: "VEHICLE_KM",
      description: "Site visit",
      supplier: null,
      amountCents: null,
      gstCents: null,
      distanceKm: "12.5",
    });
  });
});
