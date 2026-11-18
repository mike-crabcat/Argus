//! The `[agent]` section of argus.toml.
//!
//! ```toml
//! [agent]
//! provider = "openai"                 # any OpenAI-compatible API
//! base_url = "https://openrouter.ai/api/v1"
//! model = "some/model"
//! api_key_file = "/home/me/.config/argus/agent-key"   # the key alone, chmod 600
//! monthly_cap_usd = 10
//! price_input_per_mtok = 0.5          # USD per million input tokens
//! price_output_per_mtok = 1.5
//! ```
//!
//! Without the section, agent features say a model needs setting up.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;

use crate::{AgentError, ModelProvider};

/// Set this in the environment to allow `provider = "mock"` (tests only).
pub const MOCK_ENV: &str = "ARGUS_TEST_MOCK_AGENT";

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// "openai" (any OpenAI-compatible chat-completions API), or "mock"
    /// (scripted answers; tests only, needs ARGUS_TEST_MOCK_AGENT=1).
    pub provider: String,
    #[serde(default)]
    pub base_url: Option<String>,
    pub model: String,
    /// A file holding only the API key. Keeps the key out of argus.toml.
    #[serde(default)]
    pub api_key_file: Option<PathBuf>,
    /// Seconds to wait for one model answer.
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
    /// No more model calls once this month's spending reaches this.
    #[serde(default = "default_cap")]
    pub monthly_cap_usd: f64,
    /// Prices, for spend tracking: USD per million tokens.
    #[serde(default)]
    pub price_input_per_mtok: f64,
    #[serde(default)]
    pub price_output_per_mtok: f64,
    /// Tool-loop rounds before a run gives up.
    #[serde(default = "default_steps")]
    pub max_steps: usize,
    /// Summarise events (with stills) in the background.
    #[serde(default = "default_true")]
    pub summaries: bool,
    /// At most one summary per camera this often; events in between are
    /// merged into it.
    #[serde(default = "default_gap")]
    pub summary_gap_seconds: u64,
    /// For provider = "mock": a JSON file of scripted answers.
    #[serde(default)]
    pub mock_script: Option<PathBuf>,
}

fn default_timeout() -> u64 {
    120
}
fn default_cap() -> f64 {
    10.0
}
fn default_steps() -> usize {
    6
}
fn default_true() -> bool {
    true
}
fn default_gap() -> u64 {
    60
}

impl std::fmt::Debug for AgentConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentConfig")
            .field("provider", &self.provider)
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .field("api_key_file", &self.api_key_file)
            .field("monthly_cap_usd", &self.monthly_cap_usd)
            .finish_non_exhaustive()
    }
}

/// What a run costs: dollars for these tokens.
pub fn cost(cfg: &AgentConfig, input: u64, output: u64) -> f64 {
    (input as f64 * cfg.price_input_per_mtok + output as f64 * cfg.price_output_per_mtok) / 1e6
}

fn read_key(path: &Path) -> Result<String, AgentError> {
    let key = std::fs::read_to_string(path)
        .map_err(|e| AgentError::Other(format!("reading api_key_file {}: {e}", path.display())))?;
    let key = key.trim().to_owned();
    if key.is_empty() {
        return Err(AgentError::Other(format!(
            "api_key_file {} is empty",
            path.display()
        )));
    }
    Ok(key)
}

impl AgentConfig {
    /// The provider this section describes.
    pub fn provider(&self) -> Result<Arc<dyn ModelProvider>, AgentError> {
        match self.provider.as_str() {
            "openai" | "openai-compatible" => {
                let base = self
                    .base_url
                    .as_deref()
                    .unwrap_or("https://api.openai.com/v1");
                let key = self.api_key_file.as_deref().map(read_key).transpose()?;
                Ok(Arc::new(crate::openai::OpenAiCompat::new(
                    base,
                    &self.model,
                    key,
                    Duration::from_secs(self.timeout_seconds),
                )?))
            }
            "mock" => {
                if std::env::var(MOCK_ENV).as_deref() != Ok("1") {
                    return Err(AgentError::Other(format!(
                        "provider = \"mock\" is for tests; set {MOCK_ENV}=1 to use it"
                    )));
                }
                let script = match &self.mock_script {
                    Some(p) => std::fs::read_to_string(p)
                        .map_err(|e| AgentError::Other(format!("mock_script: {e}")))?,
                    None => "[]".into(),
                };
                let mut m = crate::mock::MockProvider::from_json(&script)?;
                m.cycle = true;
                Ok(Arc::new(m))
            }
            other => Err(AgentError::Other(format!(
                "unknown agent provider {other:?} (use \"openai\" for any OpenAI-compatible API)"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_redacts() {
        let c: AgentConfig = toml_like(
            r#"{"provider":"openai","base_url":"https://x/v1","model":"m","api_key_file":"/k",
                "price_input_per_mtok":2.0,"price_output_per_mtok":8.0}"#,
        );
        assert_eq!(c.monthly_cap_usd, 10.0);
        assert_eq!(c.max_steps, 6);
        assert!((cost(&c, 1_000_000, 500_000) - 6.0).abs() < 1e-9);
        let dbg = format!("{c:?}");
        assert!(dbg.contains("\"m\"") && !dbg.contains("price"));
        // No key file: a clear error, never a panic.
        let err = match c.provider() {
            Err(e) => e.to_string(),
            Ok(_) => panic!("no key file should fail"),
        };
        assert!(err.contains("api_key_file"), "{err}");
    }

    #[test]
    fn reads_the_key_file_and_rejects_unknown_providers() {
        let dir = tempfile::tempdir().unwrap();
        let k = dir.path().join("key");
        std::fs::write(&k, "  sk-test-123\n").unwrap();
        assert_eq!(read_key(&k).unwrap(), "sk-test-123");
        std::fs::write(&k, "\n").unwrap();
        assert!(read_key(&k).is_err());
        let c: AgentConfig = toml_like(r#"{"provider":"nope","model":"m"}"#);
        assert!(c.provider().is_err());
    }

    fn toml_like(json: &str) -> AgentConfig {
        serde_json::from_str(json).unwrap()
    }
}
