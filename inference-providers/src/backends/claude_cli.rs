use super::Provider;
use crate::types::{InferenceRequest, InferenceResponse, ModelInfo, ModelTier, ProviderError};
use std::process::{Command, Stdio};
use std::time::Instant;

/// Provider that delegates to the `claude` CLI using `-p` (print/non-interactive) mode.
/// Consumes Claude.ai subscription credits, not API credits.
pub struct ClaudeCliProvider {
    model: String,
    models: Vec<ModelInfo>,
}

impl ClaudeCliProvider {
    pub fn new(model: String) -> Self {
        // Priced at Haiku API rates as a proxy for subscription credit value.
        // This keeps LM Studio and Groq preferred over the CLI when available.
        let info = ModelInfo {
            provider: "claude-cli".into(),
            model: model.clone(),
            tier: ModelTier::Heavy,
            cost_per_1k_input: 0.0008,
            cost_per_1k_output: 0.004,
            context_limit: 200_000,
        };
        Self { model, models: vec![info] }
    }
}

impl Provider for ClaudeCliProvider {
    fn name(&self) -> &str { "claude-cli" }
    fn models(&self) -> &[ModelInfo] { &self.models }

    fn is_available(&self) -> bool {
        which_claude().is_some()
    }

    fn complete(&self, req: &InferenceRequest) -> Result<InferenceResponse, ProviderError> {
        let claude = which_claude()
            .ok_or_else(|| ProviderError::Unavailable("claude not found on PATH".into()))?;

        // Combine system and user into one prompt — the CLI has no separate system flag.
        let combined = format!("{}\n\n{}", req.system, req.user);

        let start = Instant::now();

        let child = Command::new(&claude)
            .args(["--model", &self.model, "-p", &combined])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| ProviderError::Unavailable(format!("spawn claude: {e}")))?;

        let output = child
            .wait_with_output()
            .map_err(|e| ProviderError::Unavailable(format!("wait claude: {e}")))?;

        let latency_ms = start.elapsed().as_millis() as u64;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            // Treat stderr containing "rate" as a rate limit so the router can back off.
            if stderr.to_lowercase().contains("rate") {
                return Err(ProviderError::RateLimit);
            }
            return Err(ProviderError::Unavailable(
                format!("claude exited {}: {stderr}", output.status)
            ));
        }

        let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if text.is_empty() {
            return Err(ProviderError::BadResponse("claude returned empty output".into()));
        }

        // The CLI doesn't report token counts; estimate from character lengths.
        let input_tokens = ((req.system.len() + req.user.len()) / 4) as u32;
        let output_tokens = (text.len() / 4) as u32;

        Ok(InferenceResponse {
            text,
            input_tokens,
            output_tokens,
            provider: "claude-cli".into(),
            model: self.model.clone(),
            latency_ms,
        })
    }
}

fn which_claude() -> Option<std::path::PathBuf> {
    let path_var = std::env::var("PATH").unwrap_or_default();
    for dir in path_var.split(':') {
        let candidate = std::path::Path::new(dir).join("claude");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}
