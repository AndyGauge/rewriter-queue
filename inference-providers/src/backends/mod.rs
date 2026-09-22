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
}
