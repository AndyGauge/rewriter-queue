use serde::Deserialize;
use std::path::PathBuf;

#[derive(Debug, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub router: RouterConfig,
    #[serde(default)]
    pub budget: BudgetConfig,
    #[serde(default)]
    pub providers: Vec<ProviderConfig>,
}

#[derive(Debug, Deserialize)]
pub struct RouterConfig {
    pub weight_cost: f64,
    pub weight_latency: f64,
    pub weight_quality: f64,
}

impl Default for RouterConfig {
    fn default() -> Self {
        Self { weight_cost: 1.0, weight_latency: 0.5, weight_quality: 2.0 }
    }
}

#[derive(Debug, Deserialize, Default)]
pub struct BudgetConfig {
    pub max_usd_per_run: Option<f64>,
    pub fallback_to_local_at: Option<f64>,
}

#[derive(Debug, Deserialize)]
pub struct ProviderConfig {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: ProviderKind,
    pub api_key_env: Option<String>,
    pub base_url: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    Anthropic,
    OpenaiCompat,
    Ollama,
    ClaudeCli,
    Gemini,
}

impl Config {
    /// Load from ~/.rewriter/config.toml. Returns default config if not found.
    pub fn load() -> Self {
        Self::load_from(&default_config_path())
    }

    pub fn load_from(path: &std::path::Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Self::default();
        };
        toml::from_str(&text).unwrap_or_else(|e| {
            eprintln!("warn: config parse error ({e}), using defaults");
            Self::default()
        })
    }
}

pub fn default_config_path() -> PathBuf {
    dirs_config().join("rewriter").join("config.toml")
}

fn dirs_config() -> PathBuf {
    // XDG_CONFIG_HOME → ~/.config on Linux, ~/Library/Application Support on macOS
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            PathBuf::from(home).join(".config")
        })
}
