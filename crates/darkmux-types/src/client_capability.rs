//! (#3494) What this darkmux's client API can do, as `GET /client/whoami`
//! lists it (`capabilities`): an app gates a feature on its capability,
//! never on a probe of how a handler happens to answer, nor on the version.
//!
//! One wire spelling per value (`snake_case`, from serde), and a new value is
//! additive: a PR that adds a client-visible route or request field adds its
//! capability here in the same PR. A darkmux that lists no `capabilities`
//! (older than #3494) has none of them. Registered with the enums that are no
//! setting (`config_enum`'s `NOT_SETTINGS`): it is a wire value.

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
}

impl ClientCapability {
    /// Every capability, in declaration order: what `whoami` lists.
    pub const ALL: [ClientCapability; 8] = [
        ClientCapability::CleanPreview,
        ClientCapability::TemplateWorkspace,
        ClientCapability::Continues,
        ClientCapability::ClonePreview,
        ClientCapability::RecipeSet,
        ClientCapability::WorkspaceImage,
        ClientCapability::WorkspaceEdit,
        ClientCapability::WorkspaceRemove,
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
