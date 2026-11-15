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
use crate::routes::{client_table, ClientRoute};

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
    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "darkmux client API",
            "version": version,
            "description": "`darkmux client serve`: 127.0.0.1 only, for apps on this machine. Every route \
                but the pairing ones takes `Authorization: Bearer <token>`. An app gates a feature on \
                `GET /client/whoami`'s `capabilities`, never on `version`."
        },
        "paths": paths,
        "components": {
            "schemas": generator.take_definitions(true),
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
            d["paths"]["/client/pair/{request_id}"]["get"]["parameters"],
            json!([{"name": "request_id", "in": "path", "required": true, "schema": {"type": "string"}}])
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
