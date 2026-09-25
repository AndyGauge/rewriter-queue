use super::Provider;
use crate::types::{InferenceRequest, InferenceResponse, ModelInfo, ModelTier, ProviderError};
use serde_json::{json, Value};
use std::time::Instant;

pub struct GeminiProvider {
    api_key: String,
    models: Vec<ModelInfo>,
}

impl GeminiProvider {
    pub fn new(api_key: String, model_ids: Vec<String>) -> Self {
        let models = model_ids
            .into_iter()
            .map(|id| model_info(&id))
            .collect();
        Self { api_key, models }
    }
}

impl Provider for GeminiProvider {
    fn name(&self) -> &str { "gemini" }
    fn models(&self) -> &[ModelInfo] { &self.models }
    fn is_available(&self) -> bool { !self.api_key.is_empty() }

    fn complete(&self, req: &InferenceRequest) -> Result<InferenceResponse, ProviderError> {
        let model = self.models.first()
            .map(|m| m.model.as_str())
            .unwrap_or("gemini-2.0-flash");

        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent",
            model
        );

        let mut body = json!({
            "contents": [{"role": "user", "parts": [{"text": req.user}]}],
            "generationConfig": {"maxOutputTokens": req.max_tokens}
        });

        if !req.system.is_empty() {
            body["system_instruction"] = json!({"parts": [{"text": req.system}]});
        }

        let start = Instant::now();

        let resp = match ureq::post(&url)
            .set("X-goog-api-key", &self.api_key)
            .set("Content-Type", "application/json")
            .send_json(&body)
        {
            Ok(r) => r,
            Err(ureq::Error::Status(429, _)) => return Err(ProviderError::RateLimit),
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                // Gemini also returns 429 inside a 200 for quota exhaustion in some cases
                if code == 429 || body.contains("RESOURCE_EXHAUSTED") {
                    return Err(ProviderError::RateLimit);
                }
                return Err(ProviderError::Unavailable(format!("HTTP {code}: {body}")));
            }
            Err(e) => return Err(ProviderError::Unavailable(e.to_string())),
        };

        let latency_ms = start.elapsed().as_millis() as u64;
        let json: Value = resp.into_json()
            .map_err(|e| ProviderError::BadResponse(e.to_string()))?;

        // Check for embedded error (Gemini sometimes returns 200 with an error body)
        if let Some(status) = json["error"]["status"].as_str() {
            if status == "RESOURCE_EXHAUSTED" {
                return Err(ProviderError::RateLimit);
            }
            return Err(ProviderError::Unavailable(
                json["error"]["message"].as_str().unwrap_or(status).to_string()
            ));
        }

        let text = json["candidates"][0]["content"]["parts"][0]["text"]
            .as_str()
            .ok_or_else(|| ProviderError::BadResponse(format!("unexpected: {json}")))?
            .to_string();

        let input_tokens = json["usageMetadata"]["promptTokenCount"]
            .as_u64().unwrap_or(0) as u32;
        let output_tokens = json["usageMetadata"]["candidatesTokenCount"]
            .as_u64().unwrap_or(0) as u32;

        Ok(InferenceResponse {
            text,
            tool_calls: Vec::new(),
            input_tokens,
            output_tokens,
            provider: "gemini".into(),
            model: model.into(),
            latency_ms,
        })
    }
}

fn model_info(model: &str) -> ModelInfo {
    // Gemini 2.0 Flash: $0.10/1M input, $0.40/1M output
    // Gemini 1.5 Flash: $0.075/1M input, $0.30/1M output
    // Gemini 1.5 Pro:   $1.25/1M input, $5.00/1M output
    let (cost_in, cost_out, tier) = match model {
        m if m.contains("flash") => (0.0001, 0.0004, ModelTier::Heavy),
        m if m.contains("pro")   => (0.00125, 0.005, ModelTier::Heavy),
        _                        => (0.0001, 0.0004, ModelTier::Heavy),
    };
    ModelInfo {
        provider: "gemini".into(),
        model: model.into(),
        tier,
        cost_per_1k_input: cost_in,
        cost_per_1k_output: cost_out,
        context_limit: 1_000_000,
    }
}
