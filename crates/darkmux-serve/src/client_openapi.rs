//! (#3494) The client API's OpenAPI 3.1 document, served at `GET
//! /client/openapi.json`.
//!
//! Built from `routes::client_table()`, never by hand: each route's path,
//! method, reply type, request body and query come from its table entry, and
//! every schema from the type's derived JSON schema, so a route, reply or
//! body that changes changes the document. `client-openapi.golden.json`
//! pins it (`client_openapi_matches_the_golden`), regenerated on purpose
//! with `DARKMUX_REGENERATE_FIXTURES=1 cargo test -p darkmux-serve
//! client_openapi`.

use schemars::generate::SchemaSettings;
use schemars::SchemaGenerator;
use serde_json::{json, Map, Value};

use crate::client_wire::{ClientError, ClientOpenApi};
use crate::routes::{client_table, ClientAuth, ClientRoute};

/// The document this build serves, built once.
pub(crate) fn served() -> &'static ClientOpenApi {
    static DOC: std::sync::OnceLock<ClientOpenApi> = std::sync::OnceLock::new();
    DOC.get_or_init(|| ClientOpenApi(document(env!("CARGO_PKG_VERSION"))))
}

/// The document for this build (`info.version` is `version`). Schemas are
/// the deserialize reading of each type, as the route-table golden reads
/// them: a field is required only when it is always present.
pub(crate) fn document(version: &str) -> Value {
    let mut generator = SchemaSettings::draft2020_12()
        .with(|s| {
            s.definitions_path = "/components/schemas".into();
            s.meta_schema = None;
        })
        .into_generator();
    let error = generator.subschema_for::<ClientError>();
    let mut paths = Map::new();
    for route in client_table() {
        let (template, parameters) = path_template(route.path);
        let item = paths.entry(template).or_insert_with(|| json!({}));
        item[route.method] = operation(&route, parameters, &mut generator);
    }
    let mut schemas = Value::Object(generator.take_definitions(true));
    plain_descriptions(&mut schemas);
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "darkmux client API",
            "version": version,
            "description": "`darkmux client serve`: listens on 127.0.0.1 only. With `clients.remote` set to \
                `tailscale`, it is also reached through this machine's `tailscale serve` entry, where a \
                request is answered only with a paired device's token and the tailnet login that device \
                paired as. Every route but the pairing ones takes the token a pairing delivered. An app \
                gates a feature on `GET /client/whoami`'s `capabilities`, never on `version`."
        },
        "security": [{"bearer": []}],
        "paths": paths,
        "components": {
            "securitySchemes": {"bearer": {"type": "http", "scheme": "bearer"}},
            "schemas": schemas,
            "responses": {
                "ClientError": {
                    "description": "Every refusal: the status says the class, `code` which refusal it is.",
                    "content": json_content(error),
                }
            }
        }
    })
}

/// An axum path as an OpenAPI template (`:name` → `{name}`), with one
/// required string parameter per `:name`.
fn path_template(path: &str) -> (String, Vec<Value>) {
    let mut parameters = Vec::new();
    let segments: Vec<String> = path
        .split('/')
        .map(|segment| match segment.strip_prefix(':') {
            Some(name) => {
                parameters.push(json!({"name": name, "in": "path", "required": true, "schema": {"type": "string"}}));
                format!("{{{name}}}")
            }
            None => segment.to_string(),
        })
        .collect();
    (segments.join("/"), parameters)
}

/// One route's operation: its parameters, its body, its success reply, and
/// the shared refusal.
fn operation(
    route: &ClientRoute,
    mut parameters: Vec<Value>,
    generator: &mut SchemaGenerator,
) -> Value {
    if let Some(query) = route.query {
        parameters.extend(query_parameters(&query(generator).to_value()));
    }
    let open = takes_no_token(route.auth, &mut parameters);
    let reply = (route.reply_schema)(generator);
    let mut op = json!({
        "responses": {
            "2XX": {"description": route.reply, "content": json_content(reply)},
            "default": {"$ref": "#/components/responses/ClientError"},
        }
    });
    if !parameters.is_empty() {
        op["parameters"] = Value::Array(parameters);
    }
    if open {
        op["security"] = json!([]);
    }
    if let Some(body) = route.body {
        op["requestBody"] =
            json!({"required": body.required, "content": json_content((body.schema)(generator))});
    }
    op
}

/// A query type's properties as `in: query` parameters.
fn query_parameters(schema: &Value) -> Vec<Value> {
    let required = |name: &str| {
        schema["required"]
            .as_array()
            .is_some_and(|r| r.iter().any(|n| n == name))
    };
    let properties = schema["properties"]
        .as_object()
        .cloned()
        .unwrap_or_default();
    properties
        .into_iter()
        .map(|(name, s)| json!({"name": name, "in": "query", "required": required(&name), "schema": s}))
        .collect()
}

/// Whether a route takes no token (the document's global `security` then
/// does not apply to it); one that takes the pairing secret gets its
/// header parameter.
fn takes_no_token(auth: ClientAuth, parameters: &mut Vec<Value>) -> bool {
    if auth == ClientAuth::PairSecret {
        parameters.push(json!({
            "name": "X-Pair-Secret", "in": "header", "required": true, "schema": {"type": "string"},
            "description": "The `pair_secret` the pairing request sent. Without it, or with another, the \
                request is unknown (404).",
        }));
    }
    auth != ClientAuth::Bearer
}

/// Rewrites every `description` under `v` as plain text: a rustdoc link
/// (`` [`x`] ``) becomes its code span ([`unlink`]).
fn plain_descriptions(v: &mut Value) {
    match v {
        Value::Object(m) => {
            if let Some(Value::String(d)) = m.get_mut("description") {
                *d = unlink(d);
            }
            m.values_mut().for_each(plain_descriptions);
        }
        Value::Array(a) => a.iter_mut().for_each(plain_descriptions),
        _ => {}
    }
}

/// `text` with each rustdoc link (`` [`x`] ``: brackets holding exactly
/// one non-empty code span, not followed by `(` or `[`, so not a Markdown
/// link) written as its code span; everything else as written.
fn unlink(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(i) = rest.find("[`") {
        out.push_str(&rest[..i]);
        match rustdoc_link(&rest[i + 2..]) {
            Some((span, tail)) => {
                out.push('`');
                out.push_str(span);
                out.push('`');
                rest = tail;
            }
            None => {
                out.push('[');
                rest = &rest[i + 1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// What follows a `` [` ``: the code span's text and what comes after its
/// `` `] ``, when the bracket is a rustdoc link.
fn rustdoc_link(after: &str) -> Option<(&str, &str)> {
    let end = after.find('`')?;
    let tail = after[end..].strip_prefix("`]")?;
    let span = &after[..end];
    let not_a_link = span.is_empty() || tail.starts_with('(') || tail.starts_with('[');
    (!not_a_link).then_some((span, tail))
}

fn json_content(schema: schemars::Schema) -> Value {
    json!({"application/json": {"schema": schema}})
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn doc() -> Value {
        document("0.0.0-test")
    }

    /// Every operation in the document, with its path and method.
    fn operations(doc: &Value) -> Vec<(String, String, Value)> {
        let mut out = Vec::new();
        for (path, item) in doc["paths"].as_object().expect("paths") {
            for (method, op) in item.as_object().expect("a path item") {
                out.push((path.clone(), method.clone(), op.clone()));
            }
        }
        out
    }

    #[test]
    fn it_is_an_openapi_3_1_document_naming_the_version() {
        let d = doc();
        assert_eq!(d["openapi"], "3.1.0");
        assert_eq!(d["info"]["version"], "0.0.0-test");
        assert!(d["info"]["title"].is_string());
    }

    /// One operation per client-table route, at the route's path with each
    /// `:param` written `{param}`.
    #[test]
    fn every_client_route_is_one_operation() {
        let d = doc();
        let ops = operations(&d);
        assert_eq!(ops.len(), crate::routes::client_table().len());
        for r in crate::routes::client_table() {
            let path = r
                .path
                .split('/')
                .map(|s| {
                    s.strip_prefix(':')
                        .map_or(s.to_string(), |p| format!("{{{p}}}"))
                })
                .collect::<Vec<_>>()
                .join("/");
            assert!(
                d["paths"][&path][r.method].is_object(),
                "{} {} is missing as {path}",
                r.method,
                r.path
            );
        }
        assert!(
            ops.iter().all(|(p, _, _)| !p.contains(':')),
            "no axum-style path remains"
        );
    }

    /// A path parameter is a required string parameter `in: path`.
    #[test]
    fn path_parameters_are_declared() {
        let d = doc();
        assert_eq!(
            d["paths"]["/client/pair/{request_id}"]["get"]["parameters"][0],
            json!({"name": "request_id", "in": "path", "required": true, "schema": {"type": "string"}})
        );
        assert_eq!(
            d["paths"]["/client/workspaces/{name}/preview"]["post"]["parameters"][0]["name"],
            "name"
        );
        assert!(
            d["paths"]["/client/whoami"]["get"]
                .get("parameters")
                .is_none(),
            "no parameters, no list"
        );
    }

    /// A query's properties are its parameters, `in: query`, required as the
    /// type requires them.
    #[test]
    fn query_parameters_come_from_the_query_type() {
        let d = doc();
        let params = d["paths"]["/client/recipe"]["get"]["parameters"]
            .as_array()
            .expect("recipe's query")
            .clone();
        let names: Vec<&str> = params.iter().map(|p| p["name"].as_str().unwrap()).collect();
        assert_eq!(names, ["workdir", "workspace"]);
        assert!(params
            .iter()
            .all(|p| p["in"] == "query" && p["required"] == false));
        assert_eq!(
            d["paths"]["/client/profiles"]["get"]["parameters"][0]["name"],
            "role"
        );
    }

    /// Every error is the one `ClientError` response, listed once.
    #[test]
    fn every_error_is_the_shared_client_error() {
        let d = doc();
        for (path, method, op) in operations(&d) {
            assert_eq!(
                op["responses"]["default"],
                json!({"$ref": "#/components/responses/ClientError"}),
                "{method} {path}"
            );
        }
        let responses = d["components"]["responses"]
            .as_object()
            .expect("components.responses");
        assert_eq!(responses.keys().collect::<Vec<_>>(), ["ClientError"]);
        assert_eq!(
            responses["ClientError"]["content"]["application/json"]["schema"],
            json!({"$ref": "#/components/schemas/ClientError"})
        );
    }

    /// A success answers with the route's reply type.
    #[test]
    fn a_success_names_the_reply_type() {
        let d = doc();
        for r in crate::routes::client_table() {
            let path = r
                .path
                .replace(":request_id", "{request_id}")
                .replace(":work_id", "{work_id}")
                .replace(":name", "{name}");
            assert_eq!(
                d["paths"][&path][r.method]["responses"]["2XX"]["content"]["application/json"]
                    ["schema"],
                json!({"$ref": format!("#/components/schemas/{}", r.reply)}),
                "{} {}",
                r.method,
                r.path
            );
        }
        let whoami = &d["components"]["schemas"]["Whoami"];
        assert!(whoami["properties"]["capabilities"].is_object(), "{whoami}");
    }

    /// A route that reads a body names its type, required unless the route
    /// reads one only when sent; a route that reads none has none.
    #[test]
    fn a_body_is_declared_where_a_route_reads_one() {
        let d = doc();
        let body = |path: &str, method: &str| d["paths"][path][method].get("requestBody").cloned();
        assert_eq!(
            body("/client/work", "post"),
            Some(
                json!({"required": true, "content": {"application/json": {"schema": {"$ref": "#/components/schemas/WorkBody"}}}})
            )
        );
        assert_eq!(
            body("/client/workspaces/{name}/preview", "post").unwrap()["required"],
            false
        );
        assert_eq!(
            body("/client/workspaces/{name}", "delete").unwrap()["required"],
            false
        );
        assert_eq!(body("/client/work/{work_id}/cancel", "post"), None);
        assert_eq!(body("/client/whoami", "get"), None);
        let work = &d["components"]["schemas"]["WorkBody"];
        assert_eq!(
            work["additionalProperties"], false,
            "an unknown field is refused: {work}"
        );
        assert!(work["properties"]["continues"].is_object());
        let recipe = &d["components"]["schemas"]["RecipeSetBody"]["properties"]["recipe"];
        assert_eq!(
            recipe["$ref"], "#/components/schemas/Recipe",
            "the recipe is checked as a Recipe"
        );
    }

    /// (#3494 review) The token is one bearer scheme, required everywhere
    /// but on the routes the table declares take none.
    #[test]
    fn the_token_is_a_bearer_scheme_every_route_but_the_pairing_ones_needs() {
        use crate::routes::ClientAuth;
        let d = doc();
        assert_eq!(
            d["components"]["securitySchemes"],
            json!({"bearer": {"type": "http", "scheme": "bearer"}})
        );
        assert_eq!(d["security"], json!([{"bearer": []}]));
        for r in crate::routes::client_table() {
            let (path, _) = path_template(r.path);
            let op = &d["paths"][&path][r.method];
            match r.auth {
                ClientAuth::Bearer => assert!(
                    op.get("security").is_none(),
                    "{} {path}: the global one",
                    r.method
                ),
                ClientAuth::Open | ClientAuth::PairSecret => {
                    assert_eq!(op["security"], json!([]), "{} {path}", r.method)
                }
            }
        }
        assert_eq!(d["paths"]["/client/pair"]["post"]["security"], json!([]));
    }

    /// (#3494 review) The pairing poll takes its secret as a required
    /// `X-Pair-Secret` header.
    #[test]
    fn the_pairing_poll_declares_its_secret_header() {
        let d = doc();
        let params = d["paths"]["/client/pair/{request_id}"]["get"]["parameters"]
            .as_array()
            .unwrap()
            .clone();
        let header: Vec<&Value> = params.iter().filter(|p| p["in"] == "header").collect();
        assert_eq!(header.len(), 1, "{params:?}");
        assert_eq!(header[0]["name"], "X-Pair-Secret");
        assert_eq!(header[0]["required"], true);
        assert_eq!(header[0]["schema"], json!({"type": "string"}));
        let headers = operations(&d)
            .iter()
            .filter(|(_, _, op)| op.to_string().contains("X-Pair-Secret"))
            .count();
        assert_eq!(headers, 1, "only the poll takes it");
    }

    /// (#3494 review) A description reads as text: no rustdoc link brackets
    /// and no Rust module paths reach the public document.
    #[test]
    fn descriptions_carry_no_rustdoc_links_or_module_paths() {
        let mut v = json!({"a": {"description": "see [`x`] and [`crate::m::f`]"}, "b": [{"description": "[`Y`]."}]});
        plain_descriptions(&mut v);
        assert_eq!(
            v,
            json!({"a": {"description": "see `x` and `crate::m::f`"}, "b": [{"description": "`Y`."}]})
        );
        let text = doc().to_string();
        for leak in ["crate::", "darkmux_types::"] {
            assert!(!text.contains(leak), "the document carries {leak}");
        }
        let mut again = doc();
        plain_descriptions(&mut again);
        assert_eq!(again, doc(), "no rustdoc link is left to strip");
    }

    /// (#3494 re-review) Only a rustdoc link, a bracket holding exactly one
    /// code span with no link target after it, loses its brackets: a code
    /// span holding a bracket, a list ending in a code span and a Markdown
    /// link are left as written.
    #[test]
    fn only_a_rustdoc_link_loses_its_brackets() {
        for (text, plain) in [
            ("[`x`]", "`x`"),
            ("a [`x`], then [`y::z`].", "a `x`, then `y::z`."),
            ("a `[` b", "a `[` b"),
            ("one of [1, `null`]", "one of [1, `null`]"),
            ("[`x`](https://example.com)", "[`x`](https://example.com)"),
            ("[`x`][ref]", "[`x`][ref]"),
            ("[``]", "[``]"),
            ("[`unclosed", "[`unclosed"),
        ] {
            assert_eq!(unlink(text), plain, "{text}");
        }
    }

    /// Every `$ref` in the document names a component it holds.
    #[test]
    fn every_reference_resolves() {
        fn refs(v: &Value, out: &mut Vec<String>) {
            match v {
                Value::Object(m) => {
                    if let Some(Value::String(r)) = m.get("$ref") {
                        out.push(r.clone());
                    }
                    m.values().for_each(|x| refs(x, out));
                }
                Value::Array(a) => a.iter().for_each(|x| refs(x, out)),
                _ => {}
            }
        }
        let d = doc();
        let mut all = Vec::new();
        refs(&d, &mut all);
        assert!(all.len() > 20, "{} references", all.len());
        for r in all {
            let pointer = r
                .strip_prefix('#')
                .unwrap_or_else(|| panic!("{r} is not local"));
            assert!(d.pointer(pointer).is_some(), "{r} does not resolve");
        }
    }

    fn golden_path() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("client-openapi.golden.json")
    }

    /// The document as served, its version aside: a route, reply or body
    /// that changes fails here until the golden is regenerated on purpose.
    #[test]
    fn client_openapi_matches_the_golden() {
        let rendered = format!(
            "{}\n",
            serde_json::to_string_pretty(&document("<darkmux version>")).unwrap()
        );
        if std::env::var_os("DARKMUX_REGENERATE_FIXTURES").is_some() {
            std::fs::write(golden_path(), &rendered).expect("writing the client OpenAPI golden");
        }
        let golden = std::fs::read_to_string(golden_path()).expect(
            "client-openapi.golden.json is missing: regenerate with DARKMUX_REGENERATE_FIXTURES=1",
        );
        assert_eq!(
            rendered, golden,
            "the client API's OpenAPI document changed. If the change is intended, regenerate with \
             `DARKMUX_REGENERATE_FIXTURES=1 cargo test -p darkmux-serve client_openapi`, add the \
             route's or field's capability (`ClientCapability`) if it is a new one, and add a CHANGELOG line"
        );
    }
}
