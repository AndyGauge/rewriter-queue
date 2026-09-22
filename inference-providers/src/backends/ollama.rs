use super::Provider;
use crate::types::{InferenceRequest, InferenceResponse, ModelInfo, ModelTier, ProviderError};
use serde_json::{json, Value};
use std::time::Instant;

pub struct OllamaProvider {
    base_url: String,
    models: Vec<ModelInfo>,
}

impl OllamaProvider {
    pub fn new(base_url: String, model_ids: Vec<String>) -> Self {
        let models = model_ids
            .into_iter()
            .map(|id| {
                let tier = infer_tier(&id);
                ModelInfo {
                    provider: "ollama".into(),
                    model: id,
                    tier,
                    cost_per_1k_input: 0.0,
                    cost_per_1k_output: 0.0,
                    // Conservative default — updated by QQ context probe at startup.
                    context_limit: 3500,
                }
            })
            .collect();
        Self { base_url, models }
    }

    /// Update the context_limit for a model after QQ probe completes.
    pub fn set_context_ok(&mut self, model: &str, ok: bool) {
        for m in &mut self.models {
            if m.model == model {
                m.context_limit = if ok { 128_000 } else { 3_500 };
            }
        }
    }
}

impl Provider for OllamaProvider {
    fn name(&self) -> &str { "ollama" }
    fn models(&self) -> &[ModelInfo] { &self.models }

    fn is_available(&self) -> bool {
        // Quick HEAD to /api/tags to check if server is up
        ureq::head(&format!("{}/api/tags", self.base_url))
            .call()
            .is_ok()
    }

    fn complete(&self, req: &InferenceRequest) -> Result<InferenceResponse, ProviderError> {
        let model = self.models.first()
            .map(|m| m.model.as_str())
            .unwrap_or("mistral");

        let body = json!({
            "model": model,
            "stream": false,
            "messages": [
                {"role": "system", "content": req.system},
                {"role": "user",   "content": req.user}
            ]
        });

        let url = format!("{}/api/chat", self.base_url);
        let start = Instant::now();

        let resp = match ureq::post(&url)
            .set("content-type", "application/json")
            .send_json(&body)
        {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                return Err(ProviderError::Unavailable(format!("HTTP {code}: {body}")));
            }
            Err(e) => return Err(ProviderError::Unavailable(e.to_string())),
        };

        let latency_ms = start.elapsed().as_millis() as u64;
        let json: Value = resp.into_json()
            .map_err(|e| ProviderError::BadResponse(e.to_string()))?;

        let text = json["message"]["content"]
            .as_str()
            .ok_or_else(|| ProviderError::BadResponse(format!("unexpected: {json}")))?
            .to_string();

        Ok(InferenceResponse {
            text,
            input_tokens: json["prompt_eval_count"].as_u64().unwrap_or(0) as u32,
            output_tokens: json["eval_count"].as_u64().unwrap_or(0) as u32,
            provider: "ollama".into(),
            model: model.into(),
            latency_ms,
        })
    }
}

/// Infer model tier from the model ID string.
/// Looks for parameter-count patterns (32b, 70b, etc.) and known model names.
fn infer_tier(model: &str) -> ModelTier {
    let lower = model.to_lowercase();

    // Explicit known models
    let heavy = [
        "qwen2.5-coder:32b", "qwen2.5-coder:32b-instruct",
        "qwen2.5:32b", "qwen2.5:72b",
        "llama3.3:70b", "llama3.1:70b", "llama3:70b",
        "deepseek-coder-v2", "deepseek-r1:32b", "deepseek-r1:70b",
        "mixtral:8x7b", "mixtral:8x22b",
        "codellama:34b", "codellama:70b",
        "command-r-plus",
    ];
    if heavy.iter().any(|&h| lower.starts_with(h) || lower == h) {
        return ModelTier::Heavy;
    }

    // Parameter-count heuristic: ≥20B → Heavy, ≥7B → Light, else Micro
    for suffix in &["b-instruct", "b_instruct", "b-chat", "b_chat", "b-q", "b:q", "b"] {
        if let Some(pos) = lower.rfind(suffix) {
            let before = &lower[..pos];
            if let Some(n_str) = before.split(|c: char| !c.is_ascii_digit()).last() {
                if let Ok(n) = n_str.parse::<u32>() {
                    return if n >= 20 {
                        ModelTier::Heavy
                    } else if n >= 7 {
                        ModelTier::Light
                    } else {
                        ModelTier::Micro
                    };
                }
            }
        }
    }

    ModelTier::Light
}
