//! Live measurements of the router against the providers in ~/.config/rewriter/config.toml.
//!
//!   cargo run --release -p inference-providers --example coin_live -- <qq|route|faults|overhead|all> [out.jsonl]

use inference_providers::backends::{ollama::OllamaProvider, openai_compat::OpenAiCompatProvider, Provider};
use inference_providers::config::{Config, ProviderKind};
use inference_providers::types::{InferenceRequest, InferenceResponse, ModelInfo, ModelTier, ProviderError};
use inference_providers::Registry;
use serde_json::{json, Value};
use std::io::Write;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

const WEIGHTS: (f64, f64, f64) = (1.0, 0.5, 2.0);
const TIERS: [ModelTier; 3] = [ModelTier::Micro, ModelTier::Light, ModelTier::Heavy];

struct Faulty {
    inner: Box<dyn Provider>,
    p_rate_limit: f64,
    p_error: f64,
    rng: Mutex<u64>,
    calls: Arc<AtomicUsize>,
}

impl Faulty {
    fn wrap(inner: Box<dyn Provider>, p_rate_limit: f64, p_error: f64, seed: u64, calls: Arc<AtomicUsize>) -> Self {
        Faulty { inner, p_rate_limit, p_error, rng: Mutex::new(seed | 1), calls }
    }

    fn uniform(&self) -> f64 {
        let mut x = self.rng.lock().unwrap();
        *x ^= *x << 13;
        *x ^= *x >> 7;
        *x ^= *x << 17;
        (*x >> 11) as f64 / (1u64 << 53) as f64
    }
}

impl Provider for Faulty {
    fn name(&self) -> &str { self.inner.name() }
    fn models(&self) -> &[ModelInfo] { self.inner.models() }
    fn is_available(&self) -> bool { self.inner.is_available() }
    fn complete(&self, req: &InferenceRequest) -> Result<InferenceResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let u = self.uniform();
        if u < self.p_rate_limit {
            return Err(ProviderError::RateLimit);
        }
        if u < self.p_rate_limit + self.p_error {
            return Err(ProviderError::Unavailable("injected".into()));
        }
        self.inner.complete(req)
    }
}

struct Instant0;

impl Provider for Instant0 {
    fn name(&self) -> &str { "instant" }
    fn models(&self) -> &[ModelInfo] {
        static M: std::sync::OnceLock<Vec<ModelInfo>> = std::sync::OnceLock::new();
        M.get_or_init(|| vec![ModelInfo {
            provider: "instant".into(),
            model: "instant".into(),
            tier: ModelTier::Heavy,
            cost_per_1k_input: 0.001,
            cost_per_1k_output: 0.002,
            context_limit: 200_000,
        }])
    }
    fn is_available(&self) -> bool { true }
    fn complete(&self, _: &InferenceRequest) -> Result<InferenceResponse, ProviderError> {
        Ok(InferenceResponse {
            text: String::new(),
            input_tokens: 0,
            output_tokens: 0,
            provider: "instant".into(),
            model: "instant".into(),
            latency_ms: 1,
        })
    }
}

fn real_providers(cfg: &Config) -> Vec<Box<dyn Provider>> {
    cfg.providers.iter().filter_map(|pc| {
        let key = pc.api_key_env.as_deref().and_then(|e| std::env::var(e).ok()).unwrap_or_default();
        let url = pc.base_url.clone().unwrap_or_default();
        match pc.kind {
            ProviderKind::OpenaiCompat => Some(Box::new(OpenAiCompatProvider::new(pc.name.clone(), url, key, pc.models.clone())) as Box<dyn Provider>),
            ProviderKind::Ollama => Some(Box::new(OllamaProvider::new(url, pc.models.clone())) as Box<dyn Provider>),
            _ => None,
        }
    }).collect()
}

fn ping(tier: ModelTier, depth: u8) -> InferenceRequest {
    InferenceRequest::new(String::new(), "Reply with the single word OK.".into(), 8, tier, depth)
}

fn snapshot_json(r: &Registry) -> Value {
    Value::Array(r.snapshot().into_iter().map(|s| json!({
        "name": s.name,
        "latency_p50_ms": s.latency_p50_ms,
        "error_rate": s.error_rate,
        "backoff": s.backoff_factor,
        "quality": s.quality_score,
        "context_ok": s.context_ok,
    })).collect())
}

fn qq(cfg: &Config, reps: usize, out: &mut dyn FnMut(Value)) {
    for rep in 0..reps {
        let r = Registry::with_providers(real_providers(cfg), WEIGHTS);
        let t = Instant::now();
        r.run_qq();
        out(json!({
            "exp": "qq", "rep": rep,
            "wall_ms": t.elapsed().as_millis() as u64,
            "providers": snapshot_json(&r),
        }));
    }
}

fn route(cfg: &Config, per_cell: usize, out: &mut dyn FnMut(Value)) {
    let r = Registry::with_providers(real_providers(cfg), WEIGHTS);
    r.run_qq();
    out(json!({ "exp": "route-init", "providers": snapshot_json(&r) }));
    for tier in TIERS {
        for depth in 0..=3u8 {
            for i in 0..per_cell {
                let t = Instant::now();
                let res = r.complete(&ping(tier, depth));
                let wall = t.elapsed().as_millis() as u64;
                out(match res {
                    Ok(resp) => json!({
                        "exp": "route", "tier": format!("{tier:?}"), "depth": depth, "i": i,
                        "provider": resp.provider, "provider_ms": resp.latency_ms, "wall_ms": wall,
                    }),
                    Err(e) => json!({
                        "exp": "route", "tier": format!("{tier:?}"), "depth": depth, "i": i,
                        "error": e.to_string(), "wall_ms": wall,
                    }),
                });
            }
        }
    }
    out(json!({ "exp": "route-final", "providers": snapshot_json(&r) }));
}

fn faults(cfg: &Config, calls_per_run: usize, pacing_ms: u64, out: &mut dyn FnMut(Value)) {
    let scenarios: [(&str, f64, f64); 4] = [
        ("baseline", 0.0, 0.0),
        ("rate-limit-30", 0.3, 0.0),
        ("error-20", 0.0, 0.2),
        ("mixed", 0.15, 0.15),
    ];
    for (label, p_rl, p_err) in scenarios {
        for depth in [0u8, 3] {
            let counters: Vec<Arc<AtomicUsize>> = cfg.providers.iter().map(|_| Arc::new(AtomicUsize::new(0))).collect();
            let providers: Vec<Box<dyn Provider>> = real_providers(cfg).into_iter().enumerate().map(|(i, p)| {
                let (rl, er) = if i == 0 { (p_rl, p_err) } else { (0.0, 0.0) };
                Box::new(Faulty::wrap(p, rl, er, 0xC0FFEE + i as u64, counters[i].clone())) as Box<dyn Provider>
            }).collect();
            let names: Vec<String> = providers.iter().map(|p| p.name().to_string()).collect();
            let r = Registry::with_providers(providers, WEIGHTS);

            let mut served: std::collections::BTreeMap<String, usize> = Default::default();
            let mut failed = 0usize;
            let t0 = Instant::now();
            for i in 0..calls_per_run {
                let who = match r.complete(&ping(ModelTier::Light, depth)) {
                    Ok(resp) => {
                        *served.entry(resp.provider.clone()).or_default() += 1;
                        resp.provider
                    }
                    Err(_) => {
                        failed += 1;
                        "FAILED".into()
                    }
                };
                out(json!({
                    "exp": "faults-trace", "scenario": label, "depth": depth, "i": i,
                    "t_ms": t0.elapsed().as_millis() as u64, "served": who, "providers": snapshot_json(&r),
                }));
                std::thread::sleep(std::time::Duration::from_millis(pacing_ms));
            }
            let attempts: usize = counters.iter().map(|c| c.load(Ordering::SeqCst)).sum();
            out(json!({
                "exp": "faults", "scenario": label, "depth": depth, "faulty": names[0], "pacing_ms": pacing_ms,
                "calls": calls_per_run, "failed": failed, "served": served,
                "attempts": attempts, "fallthroughs": attempts - (calls_per_run - failed),
                "attempts_by_provider": names.iter().zip(&counters).map(|(n, c)| (n.clone(), c.load(Ordering::SeqCst))).collect::<std::collections::BTreeMap<_, _>>(),
                "providers": snapshot_json(&r),
            }));
        }
    }
}

fn overhead(out: &mut dyn FnMut(Value)) {
    for n in [1usize, 2, 5, 10, 50, 100] {
        let r = Registry::with_providers((0..n).map(|_| Box::new(Instant0) as Box<dyn Provider>).collect(), WEIGHTS);
        let req = InferenceRequest::new(String::new(), "x".repeat(4000), 8, ModelTier::Light, 2);
        for _ in 0..1_000 {
            r.complete(&req).unwrap();
        }
        let iters = 200_000 / n.max(1);
        let t = Instant::now();
        for _ in 0..iters {
            std::hint::black_box(r.complete(&req).unwrap());
        }
        let ns = t.elapsed().as_nanos() as f64 / iters as f64;
        out(json!({ "exp": "overhead", "providers": n, "iters": iters, "ns_per_route": ns }));
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("all");
    let path = args.get(2).cloned().unwrap_or_else(|| "coin_live.jsonl".into());
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path).expect("open output");
    let mut out = |v: Value| {
        writeln!(file, "{v}").unwrap();
        if v["exp"] != "faults-trace" {
            println!("{v}");
        }
    };
    let cfg = Config::load();

    if matches!(mode, "overhead" | "all") { overhead(&mut out); }
    if matches!(mode, "qq" | "all") { qq(&cfg, 3, &mut out); }
    if matches!(mode, "route" | "all") { route(&cfg, 10, &mut out); }
    if matches!(mode, "faults" | "all") { faults(&cfg, 120, 1500, &mut out); }
}
