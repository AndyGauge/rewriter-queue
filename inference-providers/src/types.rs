use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelTier {
    Heavy,
    Light,
    Micro,
}

/// A tool a model may call during an agentic request, in whatever schema the backend's wire
/// protocol wants translated into (OpenAI's `function`, Anthropic's top-level `name`/
/// `input_schema`, etc.) -- backend-agnostic here on purpose.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    /// JSON Schema for the tool's arguments object.
    pub parameters: Value,
}

/// One invocation of a tool the model asked for. `id` is opaque, backend-assigned, and must be
/// echoed back in the matching `ToolResult` so the model can line up which result answers which
/// call (a single turn can make several calls at once).
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
}

/// The harness's answer to one `ToolCall`, keyed by its `id`.
#[derive(Debug, Clone)]
pub struct ToolResult {
    pub id: String,
    pub content: String,
}

/// One prior turn in an agentic tool-calling conversation, oldest first. An empty `history` on
/// the request means this is the first turn. Providers that don't implement tool calling
/// (`Provider::supports_tools` is `false`) are never routed a request with a non-empty
/// `tools`/`history` -- see `Registry::try_complete`.
#[derive(Debug, Clone)]
pub enum Turn {
    /// A previous response from the model: `text` (often empty when the turn was pure tool
    /// calls) plus whatever tool calls it made, if any.
    Assistant { text: String, tool_calls: Vec<ToolCall> },
    /// The harness's results for a previous `Assistant` turn's tool calls.
    ToolResults(Vec<ToolResult>),
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
    /// Tools offered for this call. Empty (the default via `new()`) is an ordinary
    /// single-shot completion, identical to before this field existed.
    pub tools: Vec<ToolDef>,
    /// Prior turns in an agentic conversation. Empty on the first call. Once non-empty,
    /// every subsequent call in the same conversation must go to the SAME provider (see
    /// `Registry::complete_pinned`) -- this wire-level history is provider-specific and
    /// can't be replayed against a different one.
    pub history: Vec<Turn>,
}

impl InferenceRequest {
    pub fn new(system: String, user: String, max_tokens: u32, tier: ModelTier, hora_depth: u8) -> Self {
        Self {
            system,
            user,
            max_tokens,
            tier,
            hora_depth,
            slot_hint: None,
            tools: Vec::new(),
            history: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct InferenceResponse {
    pub text: String,
    /// Non-empty means the model made tool calls instead of (or alongside) answering --
    /// `text` may be empty in that case. Always empty from a provider that doesn't
    /// implement tool calling.
    pub tool_calls: Vec<ToolCall>,
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
