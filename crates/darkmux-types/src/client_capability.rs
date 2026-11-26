//! (#3494) What this darkmux's client API can do, as `GET /client/whoami`
//! lists it (`capabilities`): an app gates a feature on its capability,
//! never on a probe of how a handler happens to answer, nor on the version.
//!
//! One wire spelling per value (`snake_case`, from serde), and a new value is
//! additive: a PR that adds a client-visible route or request field adds its
//! capability here in the same PR. Registered with the enums that are no
//! setting (`config_enum`'s `NOT_SETTINGS`): it is a wire value.
//!
//! **What an app reads from the list.** A darkmux whose `whoami` has a
//! `capabilities` field supports exactly the ones listed. One with no such
//! field is 5.10 or 5.11, released before the list:
//!
//! - 5.10.0 has `clean_preview`, `template_workspace`, `continues`,
//!   `workspace_edit` and `workspace_remove`;
//! - 5.11.0 also has `clone_preview`, `recipe_set` and `workspace_image`.
//!
//! An app tells those two apart by the route table, not by how a handler
//! answers: `GET /client/workspaces/{any}/recipe` is a 405
//! `method_not_allowed` on 5.11 (the path exists, for `PUT`) and a 404
//! `not_found` (`"no such route"`) on 5.10, neither needing a token
//! (`an_older_darkmux_is_told_apart_by_its_route_table` pins this build's
//! side). The preview capabilities (`clean_preview`, `clone_preview`,
//! `recipe_set`) are what this build can do, not what this machine allows:
//! an app still handles `preview_disabled` (403) when
//! `clients.preview.enabled` is off.

use serde::{Deserialize, Serialize};

/// One thing the client API can do that an app may need to check for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ClientCapability {
    /// (#3486) `POST /client/workspaces/{name}/preview` takes `{"clean":
    /// true}`: a clean restart, with a fresh install.
    CleanPreview,
    /// (#3460) `POST /client/workspaces` takes `template: true`: a new
    /// project from a template repo, which previews from its first commit.
    TemplateWorkspace,
    /// `POST /client/work` takes `continues`: a run that follows up on an
    /// earlier run of this client's in the same work copy.
    Continues,
    /// (#3571) A workspace cloned from a repo URL runs a preview, from the
    /// recipe in the commit darkmux cloned.
    ClonePreview,
    /// (#3571) `PUT /client/workspaces/{name}/recipe`: the recipe a clone's
    /// user approved, for a clone whose commit has none.
    RecipeSet,
    /// (#3545) `POST /client/workspaces/{name}/image`: an image added to a
    /// workspace.
    WorkspaceImage,
    /// (#3533) `POST /client/workspaces/{name}/edit`: a hand edit of a text
    /// span, committed like a run.
    WorkspaceEdit,
    /// (#3520) `DELETE /client/workspaces/{name}`: a workspace removed, with
    /// what darkmux made for it.
    WorkspaceRemove,
    /// (#3560) `GET /client/whoami` carries `remote`: whether this machine's
    /// `client serve` is published on its tailnet (`clients.remote
    /// tailscale`), at which HTTPS URL, or why not. What the build can do:
    /// `remote.state` says what this machine did.
    RemoteTailscale,
    /// (#3574) A model's `availability` (`GET /client/profiles`) carries a
    /// `code` whenever a dispatch refusal fits, the same code that refusal
    /// carries (`engine_in_use`, `engine_unavailable`, ...). With this
    /// listed, a missing `code` means none fits; without it, the darkmux
    /// predates codes and an app reads only `reason`.
    AvailabilityCodes,
    /// (#3560) A device on the tailnet pairs through `remote.url` (approved
    /// on the Mac, never on its own) and is then answered there with its
    /// token, from the tailnet user and device it paired as. What the build
    /// can do: `remote.state` says whether this machine is on the tailnet.
    RemoteRequests,
    /// (#3560) `POST /client/workspaces/{name}/preview/link`: a one-time link
    /// a device paired over the tailnet opens in its browser, through
    /// `remote.url`, to see that ready preview at the entry's root. What the
    /// build can do: `remote.state` says whether this machine is on the
    /// tailnet, and `preview_disabled` still answers when previews are off.
    RemotePreview,
    /// (#3569) `GET /client/engines`: the engines installed on this Mac,
    /// whether any is `usable`, and `POST /client/engines`, which adds one
    /// darkmux starts servers for once the app's user confirms.
    Engines,
    /// (#3681) `POST /client/workspaces/{name}/git`: plain git on a
    /// workspace (`status`, `fetch`, `push`, `merge_main`, `revert`,
    /// `diff`), and one lock per work copy (`work_copy_busy`).
    GitRoute,
    /// (#3682) A run whose role may neither write nor edit (`researcher`,
    /// `code-reviewer`) has its workspace mounted read-only, so it cannot
    /// change it by any means; the built-in `researcher` role exists.
    ReadOnlyRuns,
}

impl ClientCapability {
    /// Every capability, in declaration order: what `whoami` lists.
    pub const ALL: [ClientCapability; 15] = [
        ClientCapability::CleanPreview,
        ClientCapability::TemplateWorkspace,
        ClientCapability::Continues,
        ClientCapability::ClonePreview,
        ClientCapability::RecipeSet,
        ClientCapability::WorkspaceImage,
        ClientCapability::WorkspaceEdit,
        ClientCapability::WorkspaceRemove,
        ClientCapability::RemoteTailscale,
        ClientCapability::AvailabilityCodes,
        ClientCapability::RemoteRequests,
        ClientCapability::RemotePreview,
        ClientCapability::Engines,
        ClientCapability::GitRoute,
        ClientCapability::ReadOnlyRuns,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(c: ClientCapability) -> String {
        serde_json::to_value(c)
            .unwrap()
            .as_str()
            .unwrap()
            .to_string()
    }

    /// One wire spelling each, in the order `whoami` lists them (#3494).
    #[test]
    fn each_capability_has_its_one_wire_spelling_in_order() {
        let listed: Vec<String> = ClientCapability::ALL.iter().map(|c| wire(*c)).collect();
        assert_eq!(
            listed,
            [
                "clean_preview",
                "template_workspace",
                "continues",
                "clone_preview",
                "recipe_set",
                "workspace_image",
                "workspace_edit",
                "workspace_remove",
                "remote_tailscale",
                "availability_codes",
                "remote_requests",
                "remote_preview",
                "engines",
                "git_route",
                "read_only_runs",
            ]
        );
        for c in ClientCapability::ALL {
            let back: ClientCapability =
                serde_json::from_value(serde_json::json!(wire(c))).unwrap();
            assert_eq!(back, c, "{} reads back", wire(c));
        }
    }

    /// `ALL` is every variant, in declaration order: read from the derived
    /// schema, whose values follow the declaration, so a variant added
    /// without its `ALL` entry fails here.
    #[test]
    fn all_is_every_variant_in_declaration_order() {
        let schema = schemars::schema_for!(ClientCapability).to_value();
        let declared: Vec<String> = schema["oneOf"]
            .as_array()
            .expect("a documented unit enum's schema is a oneOf of constants")
            .iter()
            .map(|v| v["const"].as_str().unwrap().to_string())
            .collect();
        let listed: Vec<String> = ClientCapability::ALL.iter().map(|c| wire(*c)).collect();
        assert_eq!(listed, declared);
    }
}
