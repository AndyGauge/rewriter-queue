use crate::{
    backends::{
        anthropic::AnthropicProvider, claude_cli::ClaudeCliProvider, gemini::GeminiProvider,
        ollama::OllamaProvider, openai_compat::OpenAiCompatProvider, Provider,
    },
    config::{Config, ProviderKind},
    types::{InferenceRequest, InferenceResponse, ModelTier, ProviderError},
};
use std::sync::{Arc, Mutex};

struct ProviderMetrics {
    latency_samples: Vec<u64>, // last 20 observed latencies in ms
    recent_errors: Vec<bool>,  // last 20 calls: true = any error
    backoff_factor: f64,       // spikes on 429/413, decays on success
    quality_score: f64,        // 0.0–1.0 from QQ; 1.0 until proven otherwise
    context_ok: bool,          // passed the 4k context probe
}

impl Default for ProviderMetrics {
    fn default() -> Self {
        Self {
            latency_samples: Vec::new(),
            recent_errors: Vec::new(),
            backoff_factor: 0.0,
            quality_score: 1.0,
            context_ok: true,
        }
    }
}

impl ProviderMetrics {
    fn latency_p50_ms(&self) -> f64 {
        if self.latency_samples.is_empty() {
            return 2000.0;
        }
        let mut sorted = self.latency_samples.clone();
        sorted.sort_unstable();
        sorted[sorted.len() / 2] as f64
    }

    fn error_rate(&self) -> f64 {
        if self.recent_errors.is_empty() {
            return 0.0;
        }
        self.recent_errors.iter().filter(|&&e| e).count() as f64
            / self.recent_errors.len() as f64
    }

    fn push_error_sample(&mut self, is_error: bool) {
        self.recent_errors.push(is_error);
        if self.recent_errors.len() > 20 {
            self.recent_errors.remove(0);
        }
    }

    fn record_latency(&mut self, ms: u64) {
        self.latency_samples.push(ms);
        if self.latency_samples.len() > 20 {
            self.latency_samples.remove(0);
        }
    }

    fn record_success(&mut self) {
        self.backoff_factor = (self.backoff_factor * 0.5).max(0.0);
        self.push_error_sample(false);
    }

    fn record_rate_limit(&mut self) {
        self.backoff_factor = (self.backoff_factor * 2.0 + 1.0).min(64.0);
        self.push_error_sample(true);
    }
}

struct Slot {
    provider: Box<dyn Provider>,
    metrics: Mutex<ProviderMetrics>,
}

pub struct Registry {
    slots: Vec<Arc<Slot>>,
    weights: (f64, f64, f64), // (cost, latency, quality)
}

impl Registry {
    /// Build from config. Returns a ready registry (QQ not yet run).
    pub fn from_config(cfg: &Config) -> Self {
        let mut slots: Vec<Arc<Slot>> = Vec::new();

        for pc in &cfg.providers {
            let api_key = pc.api_key_env.as_deref()
                .and_then(|env| std::env::var(env).ok())
                .unwrap_or_default();
            let base_url = pc.base_url.clone().unwrap_or_default();
            let models = pc.models.clone();

            let provider: Box<dyn Provider> = match pc.kind {
                ProviderKind::Anthropic => {
                    Box::new(AnthropicProvider::new(api_key, models))
                }
                ProviderKind::OpenaiCompat => {
                    Box::new(OpenAiCompatProvider::new(
                        pc.name.clone(), base_url, api_key, models,
                    ))
                }
                ProviderKind::Ollama => {
                    Box::new(OllamaProvider::new(base_url, models))
                }
                ProviderKind::ClaudeCli => {
                    let model = models.into_iter().next()
                        .unwrap_or_else(|| "claude-haiku-4-5-20251001".into());
                    Box::new(ClaudeCliProvider::new(model))
                }
                ProviderKind::Gemini => {
                    Box::new(GeminiProvider::new(api_key, models))
                }
            };

            slots.push(Arc::new(Slot {
                provider,
                metrics: Mutex::new(ProviderMetrics::default()),
            }));
        }

        let r = &cfg.router;
        Registry { slots, weights: (r.weight_cost, r.weight_latency, r.weight_quality) }
    }

    /// Build from environment variables only (no config file needed).
    pub fn from_env(
        anthropic_key: Option<String>,
        ollama_host: Option<String>,
        ollama_models: Vec<String>,
    ) -> Self {
        let mut slots: Vec<Arc<Slot>> = Vec::new();

        if let Some(key) = anthropic_key {
            let models = vec!["claude-opus-4-7".into(), "claude-sonnet-4-6".into()];
            slots.push(Arc::new(Slot {
                provider: Box::new(AnthropicProvider::new(key, models)),
                metrics: Mutex::new(ProviderMetrics::default()),
            }));
        }

        if !ollama_models.is_empty() {
            let host = ollama_host.unwrap_or_else(|| "http://localhost:11434".into());
            slots.push(Arc::new(Slot {
                provider: Box::new(OllamaProvider::new(host, ollama_models)),
                metrics: Mutex::new(ProviderMetrics::default()),
            }));
        }

        Registry { slots, weights: (1.0, 0.5, 2.0) }
    }

    /// Run the Qualification Query battery against every available provider.
    /// Updates context_ok and quality_score in each provider's metrics.
    /// Skip with REWRITER_SKIP_QQ=1.
    pub fn run_qq(&self) {
        if std::env::var("REWRITER_SKIP_QQ").map(|v| v == "1").unwrap_or(false) {
            eprintln!("  [QQ] skipped (REWRITER_SKIP_QQ=1)");
            return;
        }
        eprintln!("[QQ] Qualification Query — probing {} provider(s)", self.slots.len());
        for slot in &self.slots {
            if !slot.provider.is_available() {
                eprintln!("  [QQ] {} — unavailable, skipping", slot.provider.name());
                continue;
            }
            let outcome = crate::qq::probe(&*slot.provider);
            if !outcome.rate_limited {
                let mut m = slot.metrics.lock().unwrap();
                m.context_ok = outcome.context_ok;
                m.quality_score = outcome.quality_score;
                m.record_latency(outcome.latency_p50_ms);
            }
        }
        eprintln!("[QQ] done");
    }

    /// Add Gemini provider at runtime.
    pub fn add_gemini(&mut self, api_key: String, models: Vec<String>) {
        let models = if models.is_empty() {
            vec!["gemini-2.0-flash".into()]
        } else {
            models
        };
        eprintln!("Provider: gemini ({})", models.join(", "));
        self.slots.push(Arc::new(Slot {
            provider: Box::new(GeminiProvider::new(api_key, models)),
            metrics: Mutex::new(ProviderMetrics::default()),
        }));
    }

    /// Add the Claude CLI provider at runtime.
    /// Uses `claude -p` with the given model (defaults to Haiku).
    /// Consumes Claude.ai subscription credits, not API credits.
    pub fn add_claude_cli(&mut self, model: String) {
        eprintln!("Provider: claude-cli (model={model})");
        self.slots.push(Arc::new(Slot {
            provider: Box::new(ClaudeCliProvider::new(model)),
            metrics: Mutex::new(ProviderMetrics::default()),
        }));
    }

    /// Add an OpenAI-compatible provider at runtime (e.g. Groq, OpenAI).
    pub fn add_openai_compat(
        &mut self,
        name: String,
        base_url: String,
        api_key: String,
        models: Vec<String>,
    ) {
        eprintln!("Provider: {name} ({base_url})");
        self.slots.push(Arc::new(Slot {
            provider: Box::new(OpenAiCompatProvider::new(name, base_url, api_key, models)),
            metrics: Mutex::new(ProviderMetrics::default()),
        }));
    }

    /// Count provider slots that are capable of Heavy-tier code generation.
    /// Used by the orchestrator to decide synthesis parallelism.
    /// A slot qualifies if: available, has a Heavy model, context_ok, quality >= 0.5.
    pub fn capable_heavy_slots(&self) -> usize {
        self.slots.iter()
            .filter(|s| {
                if !s.provider.is_available() { return false; }
                if !s.provider.models().iter().any(|m| m.tier == ModelTier::Heavy) { return false; }
                let m = s.metrics.lock().unwrap();
                m.context_ok && m.quality_score >= 0.5
            })
            .count()
    }

    /// Route a request to the best available provider for the given tier.
    /// Retries up to 4 times with a 65-second sleep when all providers are rate-limited.
    pub fn complete(&self, req: &InferenceRequest) -> Result<InferenceResponse, ProviderError> {
        const MAX_GLOBAL_RETRIES: u32 = 4;
        for attempt in 0..=MAX_GLOBAL_RETRIES {
            match self.try_complete(req) {
                Ok(r) => return Ok(r),
                Err(ProviderError::RateLimit) if attempt < MAX_GLOBAL_RETRIES => {
                    eprintln!(
                        "    [all providers rate limited] waiting 65s (attempt {}/{MAX_GLOBAL_RETRIES})...",
                        attempt + 1
                    );
                    std::thread::sleep(std::time::Duration::from_secs(65));
                }
                Err(e) => return Err(e),
            }
        }
        Err(ProviderError::RateLimit)
    }

    fn try_complete(&self, req: &InferenceRequest) -> Result<InferenceResponse, ProviderError> {
        let hora_depth = req.hora_depth;
        let est_input = (req.user.len() + req.system.len()) as u32 / 4;

        let candidates: Vec<&Arc<Slot>> = self.slots.iter()
            .filter(|s| s.provider.is_available())
            .filter(|s| {
                let m = s.metrics.lock().unwrap();
                m.context_ok || est_input < 3500
            })
            .filter(|s| {
                // Filter by tier capability
                s.provider.models().iter().any(|m| tier_compatible(m.tier, req.tier))
            })
            .collect();

        if candidates.is_empty() {
            return Err(ProviderError::Unavailable(format!(
                "no provider available for tier {:?} with ~{est_input} input tokens",
                req.tier
            )));
        }

        // Score each candidate
        let est_output = match req.tier {
            ModelTier::Micro  => 256u32,
            ModelTier::Light  => 1024,
            ModelTier::Heavy  => 8000,
        };

        // Sort by score, then try in order — fall through on rate limit or error.
        let mut scored: Vec<(&Arc<Slot>, f64)> = candidates
            .iter()
            .map(|s| (*s, self.score(s, est_input, est_output, hora_depth)))
            .collect();
        scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        // For parallel fan-out: rotate so the hinted slot is tried first.
        // This spreads concurrent workers across different providers rather than
        // serializing them all on the cheapest one.
        if let Some(hint) = req.slot_hint {
            let pos = hint % scored.len();
            scored.rotate_left(pos);
        }

        let mut last_err = ProviderError::Unavailable("all providers failed".into());

        for (slot, _) in &scored {
            match slot.provider.complete(req) {
                Ok(resp) => {
                    let mut m = slot.metrics.lock().unwrap();
                    m.record_latency(resp.latency_ms);
                    m.record_success();
                    return Ok(resp);
                }
                Err(ProviderError::RateLimit) => {
                    slot.metrics.lock().unwrap().record_rate_limit();
                    eprintln!("    [{}] rate limited — trying next provider", slot.provider.name());
                    last_err = ProviderError::RateLimit;
                }
                Err(e) => {
                    slot.metrics.lock().unwrap().push_error_sample(true);
                    eprintln!("    [{}] error: {e} — trying next provider", slot.provider.name());
                    last_err = e;
                }
            }
        }

        Err(last_err)
    }

    fn score(&self, slot: &Arc<Slot>, est_input: u32, est_output: u32, hora_depth: u8) -> f64 {
        let m = slot.metrics.lock().unwrap();
        let model = slot.provider.models().first();

        let price = model.map(|m| {
            m.cost_per_1k_input  * (est_input  as f64 / 1000.0)
          + m.cost_per_1k_output * (est_output as f64 / 1000.0)
        }).unwrap_or(0.01);

        let latency = m.latency_p50_ms() / 1000.0;
        let quality_penalty = 1.0 - m.quality_score;
        let backoff = m.backoff_factor;

        // Hora reliability bias: 2^depth × error_rate.
        // A provider with 15% error rate costs 8× more for a Hora-3 root task
        // than for a Hora-0 atomic task. Routes roots to reliable providers automatically.
        let reliability_bias = 2f64.powi(hora_depth as i32) * m.error_rate();

        let (wc, wl, wq) = self.weights;
        wc * price + wl * latency + wq * quality_penalty + backoff + reliability_bias
    }
}

fn tier_compatible(model_tier: ModelTier, req_tier: ModelTier) -> bool {
    // Any model can handle requests at or below its tier.
    // Heavy handles everything; Light handles Light and Micro; Micro handles only Micro.
    match (model_tier, req_tier) {
        (ModelTier::Heavy, _) => true,
        (ModelTier::Light, ModelTier::Light | ModelTier::Micro) => true,
        (ModelTier::Micro, ModelTier::Micro) => true,
        _ => false,
    }
}
