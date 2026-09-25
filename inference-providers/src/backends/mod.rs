pub mod anthropic;
pub mod claude_cli;
pub mod gemini;
pub mod ollama;
pub mod openai_compat;

use crate::types::{InferenceRequest, InferenceResponse, ModelInfo, ProviderError};

pub trait Provider: Send + Sync {
    fn name(&self) -> &str;
    fn models(&self) -> &[ModelInfo];
    fn complete(&self, req: &InferenceRequest) -> Result<InferenceResponse, ProviderError>;
    fn is_available(&self) -> bool;
    /// Whether this backend's wire protocol implements tool calling. `Registry::try_complete`
    /// never routes a request with non-empty `tools` to a provider that answers `false` here --
    /// silently ignoring `tools` would mean the model was never actually offered them.
    fn supports_tools(&self) -> bool {
        false
    }
}
