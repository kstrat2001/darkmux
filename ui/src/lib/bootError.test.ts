import { describe, it, expect, afterEach } from "vitest";
import { describeBootError } from "./bootError";

afterEach(() => {
  document.querySelectorAll('meta[name^="darkmux-"]').forEach((el) => el.remove());
});

describe("describeBootError", () => {
  it("carries the error's own message", () => {
    const content = describeBootError(new Error("boom"), "render");
    expect(content.message).toBe("boom");
  });

  it("carries the error's own stack when it has one", () => {
    const err = new Error("boom");
    const content = describeBootError(err, "render");
    expect(content.stack).toBe(err.stack);
  });

  it("normalizes a non-Error throw (e.g. a bare string) into a readable message with no stack", () => {
    const content = describeBootError("a plain string throw", "script error");
    expect(content.message).toBe("a plain string throw");
    expect(content.stack).toBeNull();
  });

  it("names which mechanism caught it, so the report distinguishes a shell render crash from a later async failure", () => {
    const rejected = describeBootError(new Error("boom"), "unhandled promise rejection");
    expect(rejected.title).toMatch(/unhandled promise rejection/);
    const rendered = describeBootError(new Error("boom"), "render");
    expect(rendered.title).toMatch(/render/);
  });

  it("reads the build line from the SAME injected meta the masthead's version chip reads", () => {
    const version = document.createElement("meta");
    version.setAttribute("name", "darkmux-version");
    version.setAttribute("content", "3.7.1");
    document.head.appendChild(version);
    const schema = document.createElement("meta");
    schema.setAttribute("name", "darkmux-flow-schema");
    schema.setAttribute("content", "9");
    document.head.appendChild(schema);

    const content = describeBootError(new Error("boom"), "render");
    expect(content.buildLine).toContain("3.7.1");
    expect(content.buildLine).toContain("9");
  });

  it("has no build line when nothing injected the meta (a harness or a daemon-less static build)", () => {
    const content = describeBootError(new Error("boom"), "render");
    expect(content.buildLine).toBeNull();
  });
});

describe("describeBootError — a live app behind the surface", () => {
  it("does not claim a failure to START when the app is already mounted", () => {
    // The raw-DOM net also fires for a LATER-tick throw, long after boot
    // succeeded. Saying "darkmux failed to start" there is a false statement
    // the operator can disprove by looking through the overlay at the app
    // still rendering underneath.
    const live = describeBootError(new Error("boom"), "script error", { appIsLive: true });
    expect(live.title).not.toMatch(/failed to start/);
    expect(live.title).toMatch(/script error/);

    const dead = describeBootError(new Error("boom"), "script error", { appIsLive: false });
    expect(dead.title).toMatch(/failed to start/);
  });

  it("tells the operator the viewer is still there, rather than that every lens is unavailable", () => {
    const live = describeBootError(new Error("boom"), "script error", { appIsLive: true });
    expect(live.hint).toMatch(/still running/i);
    expect(live.hint).not.toMatch(/every lens is unavailable/i);

    const dead = describeBootError(new Error("boom"), "script error");
    expect(dead.hint).toMatch(/every lens is unavailable/i);
  });

  it("owns the hint text so the React and raw-DOM renderers cannot drift apart", () => {
    // Both render sites read this field rather than writing the paragraph
    // out themselves; the paragraph existed twice verbatim before.
    expect(describeBootError(new Error("boom"), "render").hint.length).toBeGreaterThan(40);
  });
});
