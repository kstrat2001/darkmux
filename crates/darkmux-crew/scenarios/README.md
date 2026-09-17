# Host scenarios — simulated thermal + battery readings

A **scenario** is a machine's thermal and battery history, written down.
darkmux replays one instead of reading this Mac's real sensors, so the
thermal escalation ladder can be exercised — every tier, every transition —
without anybody having to actually overheat a laptop or run a battery to 9%.

You do not need to read or write Rust to add one. A scenario is a text file.

## The format

One **frame** per line. A frame is a reading plus how long that reading
holds. That is the whole format.

```json
{"hold_ms": 120000, "thermal": {"state": "fair"}, "battery": {"charge_pct": 45}}
```

| field | required | meaning |
|---|---|---|
| `hold_ms` | **yes** | How long this reading holds, in *simulated* milliseconds. `120000` is two minutes. Must be greater than zero. |
| `thermal.state` | no | One of `nominal`, `fair`, `serious`, `critical`. Any other string is deliberately treated as **worse than `critical`**, which is how an unrecognized future macOS state is meant to be handled. |
| `thermal.cpu_speed_limit_pct` | no | The kernel's CPU speed cap, `0`–`100`. Defaults to **100**, which means *no cap recorded* — what a cool machine reports. Write it only when the scenario is about throttling. |
| `battery.charge_pct` | **yes, if `battery` is present** | `0`–`100`. |
| `battery.on_ac` | no | Charger attached. Defaults to `false`. |
| `battery.charging` | no | Current actually flowing in. Defaults to `false`. Note this is **not** the opposite of `on_ac`: a laptop sitting at 100% on the charger is `on_ac: true, charging: false`. |
| `battery.minutes_to_empty` | no | The OS estimate, when there is one. Defaults to absent, which is the common and correct case on AC. |
| `note` | no | Free text. Ignored by the code, read by the next person. Use it to say what the frame is *for*. |

Two things that are easy to miss, and both are load-bearing:

- **Leaving `thermal` out is not the same as `nominal`.** An absent key is
  the reading the OS *failed to produce* — "time passed, no new
  information". The governor has its own handling for that, and simulating
  it is how you test it. The same goes for `battery`: leave it out entirely
  to simulate a desktop that has no battery at all.
- **The last frame holds forever.** A scenario describes a machine's
  condition, not a run's length. If the run outlasts the file, it stays at
  the last condition you wrote. So always finish with the condition you want
  the tail of the run to see.

Blank lines are ignored. Everything else must be one JSON object per line.

## Writing one

Copy [`template-annotated.jsonl`](template-annotated.jsonl) — it is a real,
runnable scenario whose `note` fields explain each frame — and edit it.

Two habits worth keeping:

1. **Start cool.** Most tiers are entered *from* a healthy state, so a
   scenario that opens at `serious` cannot test the entry.
2. **State the absence you care about.** The defects this library exists to
   catch mostly look like *nothing happening*: a duty cycle quietly
   releasing a live battery pause, a cold machine reaching a false
   `thermal-critical`. So if what should happen is "nothing", write a long
   frame of the condition and say so in the `note` — the test can then
   assert that nothing did.

## Running one against a real dispatch

```sh
DARKMUX_HOST_SOURCE_SCRIPT=/path/to/your-scenario.jsonl darkmux dispatch <role> "<message>"
```

darkmux then paces that dispatch against the scenario instead of this
machine. It will say so, loudly, in four places — `darkmux doctor`, a
warning line at dispatch start, every flow record carrying a thermal or
battery reading (`simulated_host_source`), and the run artifact. That is
deliberate: a machine reporting `nominal` while it actually cooks is worse
than no governor at all, so a simulated source is never allowed to be quiet.

The flow-record half is four builders, and the two machine-scoped ones are
why the list has to be exhaustive: a dispatch run this way acquires the
machine's host-sampler lock and becomes its sole `machine.telemetry`
emitter for the run's lifetime, so with Redis enabled those records ride
the fleet stream to your OTHER machine. Unstamped, that machine's lens
would show this laptop hitting `critical` with nothing in the data saying
otherwise.

| record | emitted by |
|---|---|
| `dispatch.rest` (pause / resume / duty cycle / breaker) | the dispatch's telemetry sampler |
| `dispatch.rest` (the rest the run actually took) | the dispatch's trajectory tailer |
| `machine.telemetry` | whichever process holds the host-sampler lock — a dispatch, or the serve daemon |
| `machine.thermal` (state transition) | the serve daemon's host sampler |

**One asymmetry to know before writing a consumer.** On a flow record the
field is ABSENT on a real run, so its mere presence answers "were these
readings real". In the run artifact's `host_window` block it is present
either way — `null` on a real run — because an artifact is read by eye long
after the run, where an explicit `null` says more than a missing key. Do
not learn the rule from one surface and apply it to the other.

It is an environment variable and **not** a `config.json` setting for the
same reason — a simulation should live as long as the shell that asked for
it, not survive reboots and upgrades.

## Adding a scenario to the test suite

Two lines of Rust, both mechanical, in `../src/host_scenario.rs`:

```rust
pub const MY_SCENARIO: &str = include_str!("../scenarios/my-scenario.jsonl");
// ...and one entry in `all()`:
("my-scenario.jsonl", MY_SCENARIO),
```

A file in this directory that is never embedded fails
`every_shipped_scenario_parses_and_none_is_silently_missing`, so a fixture
cannot quietly end up being one no test runs.
