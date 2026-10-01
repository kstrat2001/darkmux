import { describe, it, expect } from "vitest";
import { grantLine, outcomeLine, rowFacts, rowStanding, rowUid } from "./viewRows";
import type { AcceptsState } from "../../types/generated/AcceptsState";
import type { CardOutcome } from "../../types/generated/CardOutcome";
import type { FleetMachine } from "../../types/generated/FleetMachine";
import type { MachineCard } from "../../types/generated/MachineCard";
import type { MachineSpecsResponse } from "../../types/generated/MachineSpecsResponse";
import type { UnavailableWhy } from "../../types/generated/UnavailableWhy";
import type { UnreachableReason } from "../../types/generated/UnreachableReason";

const SPECS: MachineSpecsResponse = {
  darkmux_version: "5.0.0",
  flow_schema_version: "1.60.0",
  machine_id: "studio",
  machine_uid: "UID-STUDIO",
  os: "macos",
  ram_total_bytes: 34359738368,
  ram_free_for_ai_bytes: null,
  cpu_brand: "Apple M1 Max",
  loaded_models: [],
  lms_unreachable: false,
  utility_model: null,
  redis_url_redacted: null,
  generated_at_ms: 0,
};

const CARD = { specs: SPECS } as unknown as MachineCard;
const AVAILABLE: CardOutcome = { state: "available", card: CARD, source: "listener" };

function row(overrides: Partial<FleetMachine> = {}): FleetMachine {
  return {
    entry: { id: "studio", address: "100.64.0.9:8765", added_unix_ms: 1 },
    is_this_machine: false,
    machine_uid: "UID-STUDIO",
    uid_source: null,
    liveness: "no_beat",
    last_beat_ms: null,
    received_at_ms: null,
    fetch_ms: 12,
    card: AVAILABLE,
    accepts: { state: "unknown" },
    ...overrides,
  };
}

// Records keyed by the closed sets, so a variant added to the generated types
// fails to compile here until it has a line.
const UNREACHABLE: Record<UnreachableReason, string> = {
  bad_address: "bad address",
  dns_failed: "address not found",
  identity_unavailable: "identity unavailable",
  not_on_overlay: "not on the overlay network",
  pin_mismatch: "identity mismatch",
  pin_not_saved: "pin not saved",
  listener_off: "listener off",
  auth_required: "auth required",
  refused_by_peer: "refused by peer",
  listener_unavailable: "listener unavailable",
  bad_answer: "bad answer",
  unknown: "unreachable",
};
const UNAVAILABLE: Record<UnavailableWhy, string> = {
  no_card_route: "no card (older peer)",
  other_schema_major: "card schema differs",
  unparseable: "card unreadable",
  unknown: "card unavailable",
};

describe("outcomeLine: one fixed phrase per outcome", () => {
  it("an available card has no status line", () => {
    expect(outcomeLine(AVAILABLE)).toBeNull();
  });

  it.each(Object.entries(UNREACHABLE) as Array<[UnreachableReason, string]>)("unreachable %s", (reason, line) => {
    expect(outcomeLine({ state: "unreachable", reason, detail: null })).toBe(line);
  });

  it.each(Object.entries(UNAVAILABLE) as Array<[UnavailableWhy, string]>)("unavailable %s", (why, line) => {
    expect(outcomeLine({ state: "unavailable", why, peer_version: null, peer_version_source: null })).toBe(line);
  });

  it("mismatch and the catch-all each have a line", () => {
    expect(outcomeLine({ state: "mismatch", answered_as: null })).toBe("another machine answered");
    expect(outcomeLine({ state: "unknown" })).toBe("card state unknown");
  });

  it("never renders the peer's detail, an address, a uid or a token", () => {
    const detail = "connect 100.64.0.9:8765 refused, node abc.example-node.test, token sk-secret, uid UID-STUDIO";
    const lines = [
      outcomeLine({ state: "unreachable", reason: "listener_off", detail }),
      outcomeLine({ state: "unreachable", reason: "unknown", detail }),
      outcomeLine({ state: "mismatch", answered_as: "UID-STUDIO" }),
      outcomeLine({ state: "unavailable", why: "unknown", peer_version: "9.9.9", peer_version_source: null }),
    ].join(" | ");
    for (const leak of ["100.64", "example-node", "sk-secret", "UID-STUDIO", "9.9.9", "connect"]) expect(lines).not.toContain(leak);
  });
});

describe("grantLine: what a peer lets this machine do", () => {
  const accepts = (over: Partial<Extract<AcceptsState, { state: "granted" }>["accepts"]>): AcceptsState => ({
    state: "granted",
    accepts: { peer_name: "laptop", profiles: [], roles: [], images: [], workspace: false, ...over },
  });

  it("names the profiles it runs and the roles it takes here", () => {
    expect(grantLine(accepts({ profiles: ["diff-review"], roles: ["radio-host"] }))).toBe("runs diff-review · radio-host here");
  });

  it("an empty grant says so rather than showing nothing", () => {
    expect(grantLine(accepts({}))).toBe("accepts nothing");
  });

  it("every other state shows no grant", () => {
    for (const state of ["not_listed", "this_machine", "withheld", "unknown"] as const) {
      expect(grantLine({ state }), state).toBeNull();
    }
  });
});

describe("rowStanding: the view's liveness, and a card it read is proof of life", () => {
  it("the reported case: an available card with no beat is online, not offline", () => {
    expect(rowStanding(row({ liveness: "no_beat" }))).toBe("online");
    expect(rowStanding(row({ liveness: "unknown" }))).toBe("online");
  });

  it("a machine the view could not reach follows the view's own liveness", () => {
    const unreachable: CardOutcome = { state: "unreachable", reason: "listener_off", detail: null };
    expect(rowStanding(row({ card: unreachable, liveness: "live" }))).toBe("online");
    expect(rowStanding(row({ card: unreachable, liveness: "no_beat" }))).toBe("offline");
    expect(rowStanding(row({ card: unreachable, liveness: "unknown" }))).toBe("unknown");
  });

  it("this machine is never offline, whatever its card says", () => {
    expect(rowStanding(row({ is_this_machine: true, card: { state: "unknown" }, liveness: "no_beat" }))).toBe("online");
  });
});

describe("rowFacts", () => {
  it("an available peer shows its hardware and its grant, with no status note", () => {
    const facts = rowFacts(
      row({ accepts: { state: "granted", accepts: { peer_name: "laptop", profiles: ["diff-review"], roles: [], images: [], workspace: false } } }),
      new Set(),
      null,
    );
    expect(facts).toMatchObject({ spec: "Apple M1 Max · 32 GB", note: null, grant: "runs diff-review", standing: "online", isSelf: false });
  });

  it("this machine's own row shows no grant", () => {
    const facts = rowFacts(
      row({ is_this_machine: true, accepts: { state: "granted", accepts: { peer_name: "x", profiles: ["p"], roles: [], images: [], workspace: false } } }),
      new Set(),
      null,
    );
    expect(facts.grant).toBeNull();
    expect(facts.isSelf).toBe(true);
  });

  it("a peer the view could not reach shows the typed reason in place of hardware", () => {
    const facts = rowFacts(row({ card: { state: "unreachable", reason: "listener_off", detail: null }, liveness: "no_beat" }), new Set(), null);
    expect(facts).toMatchObject({ spec: "", note: "listener off", standing: "offline", grant: null });
  });

  it("a card with no chip named has no hardware line", () => {
    const noChip = { specs: { ...SPECS, cpu_brand: null } } as unknown as MachineCard;
    expect(rowFacts(row({ card: { state: "available", card: noChip, source: "listener" } }), new Set(), null).spec).toBe("");
  });
});

describe("rowUid: a flow machine that goes by a roster id", () => {
  // What `lib/machineIdentity.ts::uidForName` answers: the ONE flow uid that
  // answers to the name, in any case or `.local` spelling.
  const byName = (name: string): string | null => ({ studio: "u1", "mac-studio": "u1" })[name.toLowerCase().replace(/\.local$/, "")] ?? null;

  it("re-keys a row with no uid of its own to the flow machine that goes by its roster id", () => {
    expect(rowUid(row({ machine_uid: null }), new Set(["u1"]), null, byName)).toBe("u1");
  });

  it("never re-keys a row that carries a uid of its own", () => {
    expect(rowUid(row({ machine_uid: "OTHER" }), new Set(["u1"]), null, byName)).toBe("studio");
  });

  it("keys a row whose name no flow machine answers to by its roster id", () => {
    expect(rowUid(row({ machine_uid: null, entry: { id: "nobody", address: "a", added_unix_ms: 1 } }), new Set(["u1"]), null, byName)).toBe("nobody");
  });
});

describe("rowUid: a uid compares case-normalized", () => {
  it("returns the flow's spelling when the view spells the same uid in lower case", () => {
    expect(rowUid(row({ machine_uid: "uid-studio" }), new Set(["UID-STUDIO"]), null)).toBe("UID-STUDIO");
  });

  it("reports the machine as known to the page in either case", () => {
    expect(rowFacts(row({ machine_uid: "uid-studio" }), new Set(["UID-STUDIO"]), null).known).toBe(true);
  });

  it("does not merge two different uids", () => {
    expect(rowUid(row({ machine_uid: "UID-OTHER" }), new Set(["UID-STUDIO"]), null)).toBe("studio");
  });
});

describe("rowUid: keyed the way the rest of the page knows the machine", () => {
  it("keeps a hardware uid the page already knows", () => {
    expect(rowUid(row(), new Set(["UID-STUDIO"]), null)).toBe("UID-STUDIO");
  });

  it("keys a machine only the view knows by its roster id", () => {
    expect(rowUid(row(), new Set(), null)).toBe("studio");
  });

  it("falls back to the uid, then a fixed word, when there is no roster entry", () => {
    expect(rowUid(row({ entry: null }), new Set(), null)).toBe("UID-STUDIO");
    expect(rowUid(row({ entry: null, machine_uid: null }), new Set(), null)).toBe("unknown");
  });

  it("this machine's own row with no hardware uid takes the flow uid the page recognizes as this machine", () => {
    const self = row({ entry: null, is_this_machine: true, machine_uid: null });
    expect(rowUid(self, new Set(["u1"]), "u1")).toBe("u1");
    // A peer never borrows it.
    expect(rowUid(row({ entry: null, machine_uid: null }), new Set(["u1"]), "u1")).toBe("unknown");
  });
});
