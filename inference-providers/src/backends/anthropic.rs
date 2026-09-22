use super::Provider;
use crate::types::{InferenceRequest, InferenceResponse, ModelInfo, ModelTier, ProviderError};
use serde_json::{json, Value};
use std::time::Instant;

pub struct AnthropicProvider {
    api_key: String,
    models: Vec<ModelInfo>,
}

impl AnthropicProvider {
    pub fn new(api_key: String, model_ids: Vec<String>) -> Self {
        let models = model_ids
            .into_iter()
            .map(|id| bundled_model_info("anthropic", &id))
            .collect();
        Self { api_key, models }
    }
}

impl Provider for AnthropicProvider {
    fn name(&self) -> &str { "anthropic" }
    fn models(&self) -> &[ModelInfo] { &self.models }

    fn is_available(&self) -> bool { !self.api_key.is_empty() }

    fn complete(&self, req: &InferenceRequest) -> Result<InferenceResponse, ProviderError> {
        let model = self.models.first()
            .map(|m| m.model.as_str())
            .unwrap_or("claude-sonnet-4-6");

        let body = json!({
            "model": model,
            "max_tokens": req.max_tokens,
            "system": req.system,
            "messages": [{"role": "user", "content": req.user}]
        });

        let start = Instant::now();

        let resp = match ureq::post("https://api.anthropic.com/v1/messages")
            .set("x-api-key", &self.api_key)
            .set("anthropic-version", "2023-06-01")
            .set("content-type", "application/json")
            .send_json(&body)
        {
            Ok(r) => r,
            Err(ureq::Error::Status(429, _)) => return Err(ProviderError::RateLimit),
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                return Err(ProviderError::Unavailable(format!("HTTP {code}: {body}")));
            }
            Err(e) => return Err(ProviderError::Unavailable(e.to_string())),
        };

        let latency_ms = start.elapsed().as_millis() as u64;
        let json: Value = resp.into_json()
            .map_err(|e| ProviderError::BadResponse(e.to_string()))?;

        let text = json["content"][0]["text"]
            .as_str()
            .ok_or_else(|| ProviderError::BadResponse(format!("unexpected: {json}")))?
            .to_string();

        Ok(InferenceResponse {
            text,
            input_tokens: json["usage"]["input_tokens"].as_u64().unwrap_or(0) as u32,
            output_tokens: json["usage"]["output_tokens"].as_u64().unwrap_or(0) as u32,
            provider: "anthropic".into(),
            model: model.into(),
            latency_ms,
        })
    }
}

fn bundled_model_info(provider: &str, model: &str) -> ModelInfo {
    let (cost_in, cost_out, tier) = match model {
        "claude-opus-4-7"    => (0.015,  0.075,  ModelTier::Heavy),
        "claude-sonnet-4-6"  => (0.003,  0.015,  ModelTier::Heavy),
        "claude-haiku-4-5-20251001" => (0.0008, 0.004, ModelTier::Light),
        _                    => (0.015,  0.075,  ModelTier::Heavy),
    };
    ModelInfo {
        provider: provider.into(),
        model: model.into(),
        tier,
        cost_per_1k_input: cost_in,
        cost_per_1k_output: cost_out,
        context_limit: 200_000,
    }
}
