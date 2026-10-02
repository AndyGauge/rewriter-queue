use inference_providers::config::Config;
use inference_providers::Registry;

const DEFAULT_CLI_MODEL: &str = "claude-haiku-4-5-20251001";
const DEFAULT_LMSTUDIO_MODEL: &str = "qwen2.5-coder:32b";

#[derive(Default)]
pub struct ProviderOptions {
    pub anthropic_key: Option<String>,
    pub groq_key: Option<String>,
    pub openai_key: Option<String>,
    pub local_model: Option<String>,
    pub ollama_host: Option<String>,
    pub lmstudio_host: Option<String>,
    pub lmstudio_model: Option<String>,
    pub claude_cli_model: Option<String>,
    pub gemini_key: Option<String>,
}

pub fn claude_cli_model_from_env() -> Option<String> {
    std::env::var("REWRITER_CLAUDE_CLI_MODEL").ok().or_else(|| {
        std::env::var("REWRITER_USE_CLAUDE_CLI")
            .ok()
            .filter(|v| v == "1")
            .map(|_| DEFAULT_CLI_MODEL.into())
    })
}

impl ProviderOptions {
    pub fn from_env() -> Self {
        let var = |k: &str| std::env::var(k).ok();
        Self {
            anthropic_key: var("ANTHROPIC_API_KEY"),
            groq_key: var("GROQ_API_KEY"),
            openai_key: var("OPENAI_API_KEY"),
            lmstudio_host: var("REWRITER_LMSTUDIO_HOST"),
            lmstudio_model: var("REWRITER_LMSTUDIO_MODEL"),
            claude_cli_model: claude_cli_model_from_env(),
            gemini_key: var("GEMINI_API_KEY"),
            ..Self::default()
        }
    }
}

/// Builds the provider registry without probing it; callers that want routing quality scores
/// call `Registry::run_qq` themselves.
pub fn build_registry(cfg: &Config, opts: ProviderOptions) -> Registry {
    let mut registry = if cfg.providers.is_empty() {
        let ollama_models = opts.local_model.map(|m| vec![m]).unwrap_or_default();
        Registry::from_env(opts.anthropic_key, opts.ollama_host, ollama_models)
    } else {
        Registry::from_config(cfg)
    };
    if let Some(key) = opts.groq_key {
        registry.add_openai_compat(
            "groq".into(),
            "https://api.groq.com/openai/v1".into(),
            key,
            vec![
                "llama-3.3-70b-versatile".into(),
                "llama-3.1-8b-instant".into(),
            ],
        );
    }
    if let Some(key) = opts.openai_key {
        registry.add_openai_compat(
            "openai".into(),
            "https://api.openai.com/v1".into(),
            key,
            vec!["gpt-4o".into(), "gpt-4o-mini".into()],
        );
    }
    if let Some(host) = opts.lmstudio_host {
        let model = opts
            .lmstudio_model
            .unwrap_or_else(|| DEFAULT_LMSTUDIO_MODEL.into());
        eprintln!("LM Studio: {host} model={model}");
        registry.add_openai_compat(
            "lmstudio".into(),
            format!("{}/v1", host.trim_end_matches('/')),
            String::new(),
            vec![model],
        );
    }
    if let Some(model) = opts.claude_cli_model {
        registry.add_claude_cli(model);
    }
    if let Some(key) = opts.gemini_key {
        registry.add_gemini(key, vec!["gemini-2.0-flash".into()]);
    }
    registry
}
