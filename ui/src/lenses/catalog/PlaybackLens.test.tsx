import { describe, it, expect } from "vitest";
import { render, screen } from "@testing-library/react";
import { PlaybackLens } from "./PlaybackLens";

describe("PlaybackLens", () => {
  it("names the date in a visible not-ported notice, never a blank page", () => {
    render(<PlaybackLens date="2026-08-07" />);
    expect(screen.getByText(/lens not ported yet: playback for 2026-08-07/i)).toBeInTheDocument();
    expect(screen.getByText(/^hash: #2026-08-07$/)).toBeInTheDocument();
  });
});
