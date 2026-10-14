import { afterEach, describe, expect, it } from "vitest";
import { render, screen } from "@testing-library/react";
import { MemoryRouter, Route, Routes } from "react-router";
import { setSessionToken } from "../lib/sessionToken";
import Home from "./Home";

afterEach(() => {
  localStorage.clear();
});

function renderAtRoot() {
  render(
    <MemoryRouter initialEntries={["/"]}>
      <Routes>
        <Route path="/" element={<Home />} />
        <Route path="/app" element={<div>app area</div>} />
      </Routes>
    </MemoryRouter>,
  );
}

describe("Home", () => {
  it("shows both doors when there is no session", () => {
    renderAtRoot();
    expect(screen.getByRole("link", { name: "Log in" })).toBeInTheDocument();
    expect(screen.queryByText("app area")).not.toBeInTheDocument();
  });

  it("skips straight to /app when a session token exists", () => {
    setSessionToken("mtu_testid.test-token");
    renderAtRoot();
    expect(screen.getByText("app area")).toBeInTheDocument();
    expect(
      screen.queryByRole("link", { name: "Log in" }),
    ).not.toBeInTheDocument();
  });
});
