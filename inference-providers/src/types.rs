use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    Heavy,
    Light,
    Micro,
}

#[derive(Debug, Clone)]
pub struct InferenceRequest {
    pub system: String,
    pub user: String,
    pub max_tokens: u32,
    pub tier: ModelTier,
    /// Hora depth: 0 = atomic (cheapest provider ok), 3 = pipeline root (most reliable).
    /// Each level doubles the weight on provider error rate in the scoring function.
    pub hora_depth: u8,
    /// Preferred slot index for parallel fan-out. The registry tries this slot first
    /// among capable candidates, spreading concurrent workers across providers.
    /// None = normal cost-ranked routing.
    pub slot_hint: Option<usize>,
}

impl InferenceRequest {
    pub fn new(system: String, user: String, max_tokens: u32, tier: ModelTier, hora_depth: u8) -> Self {
        Self { system, user, max_tokens, tier, hora_depth, slot_hint: None }
    }
}

#[derive(Debug, Clone)]
pub struct InferenceResponse {
    pub text: String,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub provider: String,
    pub model: String,
    pub latency_ms: u64,
}

#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub provider: String,
    pub model: String,
    pub tier: ModelTier,
    pub cost_per_1k_input: f64,
    pub cost_per_1k_output: f64,
    /// Maximum input tokens this model reliably handles.
    /// Set to 3500 for models that fail the 4k context probe.
    pub context_limit: u32,
}

#[derive(Debug)]
pub enum ProviderError {
    RateLimit,
    Unavailable(String),
    BadResponse(String),
}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProviderError::RateLimit => write!(f, "rate limit"),
            ProviderError::Unavailable(s) => write!(f, "unavailable: {s}"),
            ProviderError::BadResponse(s) => write!(f, "bad response: {s}"),
        }
    }
}

impl std::error::Error for ProviderError {}
