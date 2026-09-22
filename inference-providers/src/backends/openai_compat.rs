use super::Provider;
use crate::types::{InferenceRequest, InferenceResponse, ModelInfo, ModelTier, ProviderError};
use serde_json::{json, Value};
use std::time::Instant;

/// Covers OpenAI, Groq, LM Studio, llamafile — any OpenAI-compatible endpoint.
pub struct OpenAiCompatProvider {
    name: String,
    base_url: String,
    api_key: String,
    models: Vec<ModelInfo>,
}

impl OpenAiCompatProvider {
    pub fn new(
        name: String,
        base_url: String,
        api_key: String,
        model_ids: Vec<String>,
    ) -> Self {
        let models = model_ids
            .into_iter()
            .map(|id| bundled_model_info(&name, &id))
            .collect();
        Self { name, base_url, api_key, models }
    }
}

impl Provider for OpenAiCompatProvider {
    fn name(&self) -> &str { &self.name }
    fn models(&self) -> &[ModelInfo] { &self.models }

    fn is_available(&self) -> bool { !self.base_url.is_empty() }

    fn complete(&self, req: &InferenceRequest) -> Result<InferenceResponse, ProviderError> {
        let model = self.models.first()
            .map(|m| m.model.as_str())
            .unwrap_or("gpt-4o");

        let body = json!({
            "model": model,
            "max_tokens": req.max_tokens,
            "messages": [
                {"role": "system", "content": req.system},
                {"role": "user",   "content": req.user}
            ]
        });

        let url = format!("{}/chat/completions", self.base_url.trim_end_matches('/'));
        let start = Instant::now();

        let mut request = ureq::post(&url)
            .set("content-type", "application/json");
        if !self.api_key.is_empty() {
            request = request.set("authorization", &format!("Bearer {}", self.api_key));
        }

        let resp = match request.send_json(&body) {
            Ok(r) => r,
            Err(ureq::Error::Status(429, _)) => return Err(ProviderError::RateLimit),
            Err(ureq::Error::Status(413, _)) => return Err(ProviderError::RateLimit),
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                return Err(ProviderError::Unavailable(format!("HTTP {code}: {body}")));
            }
            Err(e) => return Err(ProviderError::Unavailable(e.to_string())),
        };

        let latency_ms = start.elapsed().as_millis() as u64;
        let json: Value = resp.into_json()
            .map_err(|e| ProviderError::BadResponse(e.to_string()))?;

        let text = json["choices"][0]["message"]["content"]
            .as_str()
            .ok_or_else(|| ProviderError::BadResponse(format!("unexpected: {json}")))?
            .to_string();

        let input_tokens = json["usage"]["prompt_tokens"].as_u64().unwrap_or(0) as u32;
        let output_tokens = json["usage"]["completion_tokens"].as_u64().unwrap_or(0) as u32;

        Ok(InferenceResponse {
            text,
            input_tokens,
            output_tokens,
            provider: self.name.clone(),
            model: model.into(),
            latency_ms,
        })
    }
}

fn bundled_model_info(provider: &str, model: &str) -> ModelInfo {
    let lower = model.to_lowercase();
    let (cost_in, cost_out, tier, ctx) = match model {
        // Groq
        "llama-3.3-70b-versatile" => (0.00059, 0.00079, ModelTier::Heavy, 128_000),
        "llama-3.1-8b-instant"    => (0.00005, 0.00008, ModelTier::Light, 128_000),
        "mixtral-8x7b-32768"      => (0.00027, 0.00027, ModelTier::Light,  32_768),
        // OpenAI
        "gpt-4o"                  => (0.0025,  0.010,   ModelTier::Heavy, 128_000),
        "gpt-4o-mini"             => (0.00015, 0.0006,  ModelTier::Light, 128_000),
        // GX10 box (llama.cpp, self-hosted). Devstral is agentic-coding-tuned —
        // Heavy, so it's the exclusive target for code-gen roles (TargetImplementer,
        // MergeAgent). Mistral is the general-purpose/review model — Light, so it
        // covers analysis/review roles but can still be outscored by Devstral there
        // since Heavy models remain eligible for lower tiers.
        "devstral" => (0.0, 0.0, ModelTier::Heavy, 232_000),
        "mistral"  => (0.0, 0.0, ModelTier::Light, 200_000),
        // Local models served via LM Studio or any OpenAI-compat local endpoint.
        // Cost is $0; tier derived from parameter count in model name.
        _ if provider == "lmstudio" || provider == "local" => {
            let tier = infer_local_tier(&lower);
            (0.0, 0.0, tier, 32_768)
        }
        // Unknown remote model — conservative defaults
        _ => (0.001, 0.003, ModelTier::Heavy, 16_000),
    };
    ModelInfo {
        provider: provider.into(),
        model: model.into(),
        tier,
        cost_per_1k_input: cost_in,
        cost_per_1k_output: cost_out,
        context_limit: ctx,
    }
}

/// Derive tier from parameter count embedded in model name (e.g. "32b" → Heavy).
/// Coder-specialised models are promoted one tier — a 14B Rust/code model
/// is more useful for synthesis than a generic model twice its size.
fn infer_local_tier(lower: &str) -> ModelTier {
    let is_coder = lower.contains("coder") || lower.contains("code")
        || lower.contains("rust") || lower.contains("instruct");

    for word in lower.split(|c: char| !c.is_alphanumeric()) {
        if word.ends_with('b') {
            if let Ok(n) = word[..word.len() - 1].parse::<u32>() {
                return match n {
                    n if n >= 20 => ModelTier::Heavy,
                    n if n >= 7 && is_coder => ModelTier::Heavy, // promote specialised models
                    n if n >= 7 => ModelTier::Light,
                    _ => ModelTier::Micro,
                };
            }
        }
    }
    // Unknown size but it's loaded in LM Studio — assume it's capable
    if is_coder { ModelTier::Heavy } else { ModelTier::Light }
}
