"""Ambient environment the demo harness must not inherit.

darkmux refuses to start while a retired setting's env var is set
(`darkmux_types::config::RETIRED_SETTINGS` / `RENAMED_SETTINGS`), and the
operator's own shell may still export one. The demo is isolated from the
ambient shell by design, so it drops them instead of failing on them.
"""

RETIRED_ENV = (
    "DARKMUX_CREW_DIR",
    "DARKMUX_NOTEBOOK_DIR",
    "DARKMUX_RADIO_ROUTER_PROFILE",
    "DARKMUX_REMOTE_MAX_TOKENS_PER_EXECUTION",
    "DARKMUX_REMOTE_MAX_TOKENS_PER_STEP",
    "DARKMUX_REMOTE_STEP_BUDGET_POLICY",
    "DARKMUX_REMOTE_CONCURRENT_CAP",
)


def without_retired_env(env):
    """`env` (a dict) with every retired darkmux variable removed."""
    return {k: v for k, v in env.items() if k not in RETIRED_ENV}
