//! The plain OpenAI-compatible backend of the [`Engine`] facade (#316 stage 4,
//! `docs/architecture/ENGINES.md` section 9): a server darkmux reaches only
//! through its HTTP API (`mlx_lm.server`, vLLM, Splash, lithos).
//!
//! Such a server answers one question darkmux can rely on, `GET /v1/models`,
//! and that list means different things on different servers: `mlx_lm.server`
//! lists every MLX model in the Hugging Face cache plus the one it was started
//! with (`handle_models_request`, mlx-lm 0.32.1), and loads whichever model a
//! request names, one at a time. So the list is the [`Engine::catalog`], what
//! the server would answer to, and never what it holds: [`Engine::loaded`] and
//! [`Engine::resident_ids`] are [`NotReported`], never an empty list, and the
//! ledger keeps its residents unknown. The engine has no load lifecycle darkmux
//! drives ([`Engine::host`] is `None`) and nothing darkmux may unload (ENGINES.md
//! open questions 1 and 3).

use super::{Capabilities, CatalogFacts, Engine, Loading, ModelMeta, NotReported, Residency};
use crate::model_ledger::ModelLedger;
use anyhow::{Context, Result};
use darkmux_gestalt::ModelHost;
use darkmux_types::LoadedModel;
use std::time::Duration;

/// Bound on one `GET /v1/models`, connect through the last byte. The same
/// bound the ledger gives each `lms` metadata call (`LMS_PROBE_BOUND`), for
/// the same reason: a wedged server is reported rather than hanging `doctor`,
/// `init` or the ledger. Not measured against a slow server yet.
pub const MODELS_PROBE_BOUND: Duration = Duration::from_secs(5);

/// A server reached at `base_url`, the endpoint's `/v1` root (the `url` of a
/// `profiles.json` endpoint), named `name` in every message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiCompatible {
    name: String,
    base_url: String,
}

impl OpenAiCompatible {
    pub fn new(name: impl Into<String>, base_url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
        }
    }

    fn not_reported(&self, question: &'static str) -> anyhow::Error {
        NotReported {
            engine: self.name.clone(),
            question,
        }
        .into()
    }

    /// The `id` of each model `GET {base_url}/models` lists, in its order.
    fn served_ids(&self) -> Result<Vec<String>> {
        let url = format!("{}/models", self.base_url);
        let agent = ureq::AgentBuilder::new()
            .redirects(0)
            .timeout_connect(MODELS_PROBE_BOUND)
            .build();
        let text = agent
            .get(&url)
            .timeout(MODELS_PROBE_BOUND)
            .call()
            .with_context(|| format!("{}: GET {url}", self.name))?
            .into_string()
            .with_context(|| format!("{}: reading GET {url}", self.name))?;
        let body: serde_json::Value = serde_json::from_str(&text)
            .with_context(|| format!("{}: GET {url} did not return JSON", self.name))?;
        parse_model_ids(&body).with_context(|| format!("{}: GET {url}", self.name))
    }
}

/// The ids in an OpenAI `{"object":"list","data":[{"id":…}]}` body. A body
/// without a `data` array, or an entry without a string `id`, is an error, not
/// a shorter list.
fn parse_model_ids(body: &serde_json::Value) -> Result<Vec<String>> {
    let data = body
        .get("data")
        .and_then(|d| d.as_array())
        .context("the response has no `data` array")?;
    data.iter()
        .map(|m| {
            m.get("id")
                .and_then(|id| id.as_str())
                .map(str::to_string)
                .context("a `data` entry has no string `id`")
        })
        .collect()
}

impl Engine for OpenAiCompatible {
    /// The most conservative declaration: a list of ids and nothing else.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            residency: Residency::NotReported,
            loading: Loading::Unknown,
            unload: false,
            ownership_mark: false,
            catalog: CatalogFacts { size: false, context_limit: false, tool_training: false },
        }
    }

    fn name(&self) -> String {
        self.name.clone()
    }

    fn loaded(&self) -> Result<Vec<LoadedModel>> {
        Err(self.not_reported("which models it has loaded"))
    }

    /// One row per listed model. The list carries only an id, so the row's
    /// size is 0 (unknown), its context limit `None`, its tool training
    /// unknown (`false`), and its publisher the id's `owner/` prefix when it
    /// has one.
    fn catalog(&self) -> Result<Vec<ModelMeta>> {
        Ok(self
            .served_ids()?
            .into_iter()
            .map(|id| ModelMeta {
                publisher: id
                    .split_once('/')
                    .map(|(owner, _)| owner.to_string())
                    .unwrap_or_default(),
                display_name: id.clone(),
                model_key: id,
                size_bytes: 0,
                params_string: None,
                architecture: None,
                max_context_length: None,
                trained_for_tool_use: false,
                model_type: "llm".to_string(),
            })
            .collect())
    }

    fn unload(&self, _identifier: &str) -> Result<()> {
        Err(self.not_reported("a way to unload a model"))
    }

    fn host(&self) -> Option<Box<dyn ModelHost>> {
        None
    }

    fn resident_ids(&self) -> Result<Vec<String>> {
        Err(self.not_reported("which models it has loaded"))
    }

    fn version(&self) -> Option<String> {
        None
    }

    fn ledger(&self) -> ModelLedger {
        crate::model_ledger::gather_residents_not_reported(&format!(
            "{} does not report which models it has loaded",
            self.name
        ))
    }

    fn binary(&self) -> Option<(String, darkmux_types::config_access::Source)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::is_not_reported;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Serve `response` (status line onward) to each of `requests`
    /// connections on a loopback port, returning the `/v1` base URL and the
    /// request lines received.
    fn serve(
        response: &'static str,
        requests: usize,
    ) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}/v1", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut lines = Vec::new();
            for _ in 0..requests {
                let (mut stream, _) = listener.accept().unwrap();
                let mut buf = [0u8; 4096];
                let n = stream.read(&mut buf).unwrap();
                let req = String::from_utf8_lossy(&buf[..n]).into_owned();
                lines.push(req.lines().next().unwrap_or_default().to_string());
                stream.write_all(response.as_bytes()).unwrap();
            }
            lines
        });
        (base, handle)
    }

    fn ok_json(body: &'static str) -> &'static str {
        Box::leak(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            )
            .into_boxed_str(),
        )
    }

    /// The shape `mlx_lm.server` 0.32.1 answers: cached repos plus the
    /// started model's resolved path.
    const MLX_MODELS: &str = r#"{"object":"list","data":[{"id":"mlx-community/Qwen3-30B-A3B-4bit","object":"model","created":1},{"id":"/models/glimmer","object":"model","created":1}]}"#;

    #[test]
    fn catalog_is_every_listed_model_with_unknown_facts() {
        let (base, server) = serve(ok_json(MLX_MODELS), 1);
        let engine = OpenAiCompatible::new("mlx", format!("{base}/"));
        let catalog = engine.catalog().unwrap();
        assert_eq!(
            server.join().unwrap(),
            vec!["GET /v1/models HTTP/1.1".to_string()]
        );
        let rows: Vec<_> = catalog
            .iter()
            .map(|m| {
                (
                    m.model_key.as_str(),
                    m.publisher.as_str(),
                    m.size_bytes,
                    m.max_context_length,
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                ("mlx-community/Qwen3-30B-A3B-4bit", "mlx-community", 0, None),
                ("/models/glimmer", "", 0, None),
            ]
        );
    }

    /// The list is what the server would answer to, never what it holds: the
    /// residency questions are typed as not reported, not answered empty.
    #[test]
    fn residency_and_unload_are_not_reported_never_empty() {
        let engine = OpenAiCompatible::new("mlx", "http://127.0.0.1:9/v1");
        for err in [
            engine.loaded().map(|_| ()).unwrap_err(),
            engine.resident_ids().map(|_| ()).unwrap_err(),
            engine.unload("m").unwrap_err(),
        ] {
            assert!(is_not_reported(&err), "{err:#}");
            assert!(err.to_string().starts_with("mlx does not report"), "{err}");
        }
        assert!(engine.host().is_none());
        assert!(engine.binary().is_none());
        assert!(engine.version().is_none());
    }

    #[test]
    fn its_declaration_matches_what_it_does() {
        let (base, server) = serve(ok_json(MLX_MODELS), 1);
        crate::engine::assert_declaration_holds(&OpenAiCompatible::new("mlx", base), "m");
        server.join().unwrap();
    }

    #[test]
    fn a_body_that_is_not_a_model_list_is_an_error_not_an_empty_catalog() {
        for body in [r#"{"object":"list"}"#, r#"{"data":[{"object":"model"}]}"#] {
            let err = parse_model_ids(&serde_json::from_str(body).unwrap()).unwrap_err();
            assert!(!is_not_reported(&err), "{err:#}");
        }
        assert_eq!(
            parse_model_ids(&serde_json::json!({"data": []})).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn an_error_status_is_an_error_naming_the_engine() {
        let (base, server) = serve(
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            1,
        );
        let err = OpenAiCompatible::new("vllm", base).catalog().unwrap_err();
        server.join().unwrap();
        assert!(
            format!("{err:#}").starts_with("vllm: GET http://127.0.0.1:"),
            "{err:#}"
        );
    }

    /// The ledger keeps residents unknown and says which engine could not
    /// answer, so its memory never reads as an empty machine (#3074).
    #[test]
    fn ledger_says_residents_are_unknown_and_why() {
        let ledger = OpenAiCompatible::new("mlx", "http://127.0.0.1:9/v1").ledger();
        assert!(ledger.models.is_empty());
        // (#316 stage 4.5) Said as information: nothing is wrong, so the
        // machine page shows no WARN for it.
        assert!(
            ledger.messages.iter().any(|m| m
                .text
                .starts_with("mlx does not report which models it has loaded")
                && m.severity == crate::model_ledger::Severity::Info),
            "{:?}",
            ledger.messages
        );
        assert_eq!(
            ledger.attribution,
            crate::model_ledger::Attribution::Unavailable
        );
        assert_eq!(
            ledger.attribution_note,
            "mlx does not report which models it has loaded; worker footprint not attributed"
        );
    }
}
