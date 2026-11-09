//! (#3468) The machine-readable code on every refusal of `darkmux client
//! serve` and on every failed client work item, beside the sentence for a
//! person: an app branches on the code, never on the wording.
//!
//! One wire spelling per value (`snake_case`, from serde), and a new value
//! is additive. A failed run's code is the one its `dispatch.error` record
//! carries (`payload.code`), read through [`ClientCode::of_dispatch`], never
//! re-derived from the failure's text. Registered with the enums that are no
//! setting (`config_enum`'s `NOT_SETTINGS`): it is a wire value.

use serde::{Deserialize, Serialize};

/// Why the client API refused a request, or why a work item failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClientCode {
    // ── The request itself ──
    /// Not from an app on this machine (a proxy, another host, a web page).
    NotLocal,
    UnsupportedMediaType,
    BadRequest,
    PayloadTooLarge,
    /// No such route, id or workspace.
    NotFound,
    MethodNotAllowed,
    /// A missing or unknown bearer token.
    Unauthorized,
    /// Outside what this machine lets an app run (a profile, a workdir).
    Forbidden,
    /// The state does not allow it now (not running, already undone).
    Conflict,
    /// Valid JSON that cannot be acted on as asked.
    Unprocessable,
    /// Too many pending pairings or running work items.
    TooManyRequests,
    /// Not now: this machine's config could not be read, or `client serve`
    /// is stopping (send the request again once it is back).
    Unavailable,
    /// darkmux failed; its log says why.
    Internal,

    // ── Previews ──
    /// `clients.preview.enabled` is off on this machine (403).
    PreviewDisabled,
    /// The workspace was cloned, or is no longer a worktree of the user's
    /// checkout: no checkout of theirs to read a recipe from (404).
    NoSourceCheckout,
    /// A template project whose first commit, the recipe a preview of it
    /// trusts (#3460), has no `.darkmux/preview.json` (404).
    TemplateNoRecipe,
    /// No `.darkmux/preview.json` in the user's checkout (404).
    NoRecipe,
    /// The recipe cannot be used as written (422).
    RecipeInvalid,

    // ── Workspaces ──
    /// The repository's own git config defines a filter driver darkmux
    /// does not run (#3457).
    FilterInRepoConfig,
    /// The checkout's own git config sets a network key darkmux does not
    /// read; the fetch was refused (#3457).
    NetworkKeyInRepoConfig,
    /// Fetching the checkout's remote failed for another reason.
    FetchFailed,
    /// Cloning the repository failed.
    CloneFailed,

    // ── A failed run, from its `dispatch.error` `code` ──
    /// The memory is held by a running dispatch; retry later.
    Busy,
    /// The model does not fit this machine; retrying cannot help.
    WontFit,
    /// The engine server would not start, or exited.
    ModelStartFailed,
    /// darkmux's own engine server would not stop to make way.
    ModelStopFailed,
    /// A server darkmux did not start holds the endpoint's port.
    PortInUse,
    /// No engine binary, or a version darkmux was not verified against.
    EngineUnavailable,
    /// The engine's own server is running (rec runs one per machine).
    EngineInUse,
    /// The run failed with no code of its own (it exited, was killed, or
    /// printed no result).
    DispatchFailed,
}

impl ClientCode {
    /// A failed run's code from its `dispatch.error` `code` (`None`: the
    /// record names none, or there is no record): the same spelling, read.
    /// A code this build does not know is [`ClientCode::DispatchFailed`].
    pub fn of_dispatch(code: Option<&str>) -> ClientCode {
        code.and_then(|c| {
            serde_json::from_value::<ClientCode>(serde_json::Value::String(c.to_string())).ok()
        })
        .filter(|c| c.is_dispatch_failure())
        .unwrap_or(ClientCode::DispatchFailed)
    }

    /// Whether this is a code a failed run can carry.
    pub fn is_dispatch_failure(self) -> bool {
        use ClientCode::*;
        matches!(
            self,
            Busy | WontFit
                | ModelStartFailed
                | ModelStopFailed
                | PortInUse
                | EngineUnavailable
                | EngineInUse
                | DispatchFailed
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(c: ClientCode) -> String {
        serde_json::to_value(c)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string()
    }

    /// The issue's list, one wire spelling each (#3468).
    #[test]
    fn each_code_has_its_one_wire_spelling() {
        use ClientCode::*;
        for (c, s) in [
            (PreviewDisabled, "preview_disabled"),
            (NoSourceCheckout, "no_source_checkout"),
            (TemplateNoRecipe, "template_no_recipe"),
            (NoRecipe, "no_recipe"),
            (RecipeInvalid, "recipe_invalid"),
            (Busy, "busy"),
            (WontFit, "wont_fit"),
            (ModelStartFailed, "model_start_failed"),
            (PortInUse, "port_in_use"),
            (EngineUnavailable, "engine_unavailable"),
            (EngineInUse, "engine_in_use"),
            (FilterInRepoConfig, "filter_in_repo_config"),
            (NetworkKeyInRepoConfig, "network_key_in_repo_config"),
            (CloneFailed, "clone_failed"),
        ] {
            assert_eq!(wire(c), s);
            assert_eq!(
                serde_json::from_value::<ClientCode>(serde_json::json!(s)).unwrap(),
                c,
                "{s} reads back"
            );
        }
    }

    /// A failed run's code is its `dispatch.error` code, read as spelled; a
    /// code this build does not know, or a refusal code that is no run
    /// failure, or none, is `dispatch_failed`.
    #[test]
    fn a_failed_runs_code_is_read_from_its_dispatch_error() {
        use ClientCode::*;
        for (raw, c) in [
            ("busy", Busy),
            ("wont_fit", WontFit),
            ("model_start_failed", ModelStartFailed),
            ("model_stop_failed", ModelStopFailed),
            ("port_in_use", PortInUse),
            ("engine_unavailable", EngineUnavailable),
            ("engine_in_use", EngineInUse),
        ] {
            assert_eq!(ClientCode::of_dispatch(Some(raw)), c, "{raw}");
        }
        assert_eq!(
            ClientCode::of_dispatch(Some("from_a_newer_darkmux")),
            DispatchFailed
        );
        assert_eq!(
            ClientCode::of_dispatch(Some("preview_disabled")),
            DispatchFailed,
            "not a run failure"
        );
        assert_eq!(ClientCode::of_dispatch(None), DispatchFailed);
    }
}
