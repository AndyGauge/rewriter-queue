use super::*;
use crate::types::ModelInfo;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

type Built = (Registry, Vec<Arc<AtomicUsize>>, Arc<Mutex<Vec<String>>>);
type Handler = Box<dyn Fn(&InferenceRequest) -> Result<InferenceResponse, ProviderError> + Send + Sync>;

struct Mock {
    name: String,
    models: Vec<ModelInfo>,
    available: bool,
    calls: Arc<AtomicUsize>,
    log: Arc<Mutex<Vec<String>>>,
    handler: Handler,
}

impl Provider for Mock {
    fn name(&self) -> &str { &self.name }
    fn models(&self) -> &[ModelInfo] { &self.models }
    fn is_available(&self) -> bool { self.available }
    fn complete(&self, req: &InferenceRequest) -> Result<InferenceResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.log.lock().unwrap().push(self.name.clone());
        (self.handler)(req)
    }
}

fn model(tier: ModelTier, cost_in: f64, cost_out: f64) -> ModelInfo {
    ModelInfo {
        provider: "mock".into(),
        model: "mock".into(),
        tier,
        cost_per_1k_input: cost_in,
        cost_per_1k_output: cost_out,
        context_limit: 200_000,
    }
}

fn reply(text: &str, latency_ms: u64) -> Result<InferenceResponse, ProviderError> {
    Ok(InferenceResponse {
        text: text.into(),
        input_tokens: 0,
        output_tokens: 0,
        provider: "mock".into(),
        model: "mock".into(),
        latency_ms,
    })
}

struct Builder {
    log: Arc<Mutex<Vec<String>>>,
    providers: Vec<Box<dyn Provider>>,
    calls: Vec<Arc<AtomicUsize>>,
}

impl Builder {
    fn new() -> Self {
        Builder { log: Arc::new(Mutex::new(Vec::new())), providers: Vec::new(), calls: Vec::new() }
    }

    fn add(
        mut self,
        name: &str,
        models: Vec<ModelInfo>,
        available: bool,
        handler: impl Fn(&InferenceRequest) -> Result<InferenceResponse, ProviderError> + Send + Sync + 'static,
    ) -> Self {
        let calls = Arc::new(AtomicUsize::new(0));
        self.calls.push(calls.clone());
        self.providers.push(Box::new(Mock {
            name: name.into(),
            models,
            available,
            calls,
            log: self.log.clone(),
            handler: Box::new(handler),
        }));
        self
    }

    fn ok(self, name: &str, tier: ModelTier, cost_out: f64) -> Self {
        self.add(name, vec![model(tier, 0.0, cost_out)], true, |_| reply("ok", 2000))
    }

    fn build(self) -> Built {
        (Registry::with_providers(self.providers, (1.0, 0.5, 2.0)), self.calls, self.log)
    }
}

fn req(tier: ModelTier, input_chars: usize, depth: u8) -> InferenceRequest {
    InferenceRequest::new(String::new(), "x".repeat(input_chars), 64, tier, depth)
}

fn set_error_rate(r: &Registry, slot: usize, errors: usize, window: usize, now: Instant) {
    let mut m = r.slots[slot].metrics.lock().unwrap();
    m.recent_errors.clear();
    for i in 0..window {
        m.push_error_sample(i < errors, now);
    }
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-6
}

const TIERS: [ModelTier; 3] = [ModelTier::Micro, ModelTier::Light, ModelTier::Heavy];

fn rank(t: ModelTier) -> u8 {
    match t {
        ModelTier::Micro => 0,
        ModelTier::Light => 1,
        ModelTier::Heavy => 2,
    }
}

// ── Eligibility gate ────────────────────────────────────────────────────────

#[test]
fn tier_compatibility_is_downward_closure_of_chain() {
    for m in TIERS {
        for r in TIERS {
            assert_eq!(tier_compatible(m, r), rank(r) <= rank(m), "model {m:?} request {r:?}");
        }
    }
}

#[test]
fn gate_is_sound_and_complete_over_all_combinations() {
    for model_tier in TIERS {
        for req_tier in TIERS {
            for available in [true, false] {
                for context_ok in [true, false] {
                    for chars in [0usize, 13_996, 14_000, 40_000] {
                        let (r, calls, _) = Builder::new()
                            .add("p", vec![model(model_tier, 0.0, 0.0)], available, |_| reply("ok", 1))
                            .build();
                        r.slots[0].metrics.lock().unwrap().context_ok = context_ok;

                        let est_input = chars as u32 / 4;
                        let eligible = available
                            && (context_ok || est_input < 3500)
                            && rank(req_tier) <= rank(model_tier);

                        let result = r.try_complete(&req(req_tier, chars, 0));
                        assert_eq!(result.is_ok(), eligible);
                        assert_eq!(calls[0].load(Ordering::SeqCst), eligible as usize);
                        if !eligible {
                            assert!(matches!(result, Err(ProviderError::Unavailable(_))));
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn context_limited_provider_is_bypassed_at_3500_token_boundary() {
    let (r, calls, _) = Builder::new()
        .ok("free-small", ModelTier::Heavy, 0.0)
        .ok("paid-large", ModelTier::Heavy, 1.0)
        .build();
    r.slots[0].metrics.lock().unwrap().context_ok = false;

    r.try_complete(&req(ModelTier::Heavy, 13_996, 0)).unwrap();
    assert_eq!(calls[0].load(Ordering::SeqCst), 1);

    r.try_complete(&req(ModelTier::Heavy, 14_000, 0)).unwrap();
    assert_eq!(calls[0].load(Ordering::SeqCst), 1);
    assert_eq!(calls[1].load(Ordering::SeqCst), 1);
}

// ── Cost function ───────────────────────────────────────────────────────────

#[test]
fn score_is_monotone_in_each_metric() {
    let now = Instant::now();
    let score_with = |cost_out: f64, latency: u64, quality: f64, backoff: f64, errors: usize, depth: u8| {
        let (r, _, _) = Builder::new().ok("p", ModelTier::Heavy, cost_out).build();
        {
            let mut m = r.slots[0].metrics.lock().unwrap();
            m.record_latency(latency);
            m.quality_score = quality;
            m.backoff_factor = backoff;
            m.backoff_at = now;
        }
        set_error_rate(&r, 0, errors, 20, now);
        r.score(&r.slots[0], 100, 1024, depth, now)
    };

    let base = (0.01, 1000u64, 0.8, 1.0, 2usize, 1u8);
    for step in 1..10 {
        let k = step as f64;
        let (c, l, q, b, e, d) = base;
        assert!(score_with(c * k, l, q, b, e, d) >= score_with(c, l, q, b, e, d));
        assert!(score_with(c, l * step, q, b, e, d) >= score_with(c, l, q, b, e, d));
        assert!(score_with(c, l, q / k, b, e, d) >= score_with(c, l, q, b, e, d));
        assert!(score_with(c, l, q, b * k, e, d) >= score_with(c, l, q, b, e, d));
        assert!(score_with(c, l, q, b, e + step as usize, d) >= score_with(c, l, q, b, e, d));
        assert!(score_with(c, l, q, b, e, d + step as u8) >= score_with(c, l, q, b, e, d));
    }
}

#[test]
fn score_decomposes_into_documented_terms() {
    let (r, _, _) = Builder::new()
        .add("p", vec![model(ModelTier::Heavy, 0.002, 0.01)], true, |_| reply("ok", 1))
        .build();
    let now = Instant::now();
    {
        let mut m = r.slots[0].metrics.lock().unwrap();
        m.record_latency(1500);
        m.quality_score = 0.75;
        m.backoff_factor = 3.0;
        m.backoff_at = now;
    }
    set_error_rate(&r, 0, 4, 20, now);

    let price = 0.002 * 2.0 + 0.01 * 1.024;
    let expected = 1.0 * price + 0.5 * 1.5 + 2.0 * 0.25 + 3.0 + 8.0 * 0.2;
    assert!((r.score(&r.slots[0], 2000, 1024, 3, now) - expected).abs() < 1e-12);
}

#[test]
fn depth_dominance_threshold_matches_closed_form() {
    // A: free but 25% error rate. B: 1.0/1k-out and 5% error rate.
    // ΔR = 1.024 (Light tier ⇒ 1024 est. output tokens); Δe = 0.20.
    // B wins iff 2^d · Δe > ΔR  ⇒  d* = ⌈log2(ΔR/Δe)⌉ = ⌈log2 5.12⌉ = 3.
    let d_star = (1.024f64 / 0.20).log2().ceil() as u8;
    assert_eq!(d_star, 3);

    for depth in 0..=8u8 {
        let (r, calls, _) = Builder::new()
            .ok("cheap-flaky", ModelTier::Light, 0.0)
            .ok("paid-reliable", ModelTier::Light, 1.0)
            .build();
        set_error_rate(&r, 0, 5, 20, Instant::now());
        set_error_rate(&r, 1, 1, 20, Instant::now());

        r.try_complete(&req(ModelTier::Light, 0, depth)).unwrap();
        let reliable_chosen = calls[1].load(Ordering::SeqCst) == 1;
        assert_eq!(reliable_chosen, depth >= d_star, "depth {depth}");
    }
}

#[test]
fn equal_error_rates_make_depth_irrelevant() {
    for depth in 0..=8u8 {
        let (r, calls, _) = Builder::new()
            .ok("cheap", ModelTier::Light, 0.0)
            .ok("paid", ModelTier::Light, 1.0)
            .build();
        let now = Instant::now();
        set_error_rate(&r, 0, 3, 20, now);
        set_error_rate(&r, 1, 3, 20, now);
        r.try_complete(&req(ModelTier::Light, 0, depth)).unwrap();
        assert_eq!(calls[0].load(Ordering::SeqCst), 1, "depth {depth}");
    }
}

#[test]
fn price_is_taken_from_first_listed_model() {
    let (r, _, _) = Builder::new()
        .add(
            "multi",
            vec![model(ModelTier::Heavy, 0.0, 1.0), model(ModelTier::Micro, 0.0, 0.0)],
            true,
            |_| reply("ok", 1),
        )
        .build();
    let with_heavy_price = r.score(&r.slots[0], 0, 256, 0, Instant::now());
    let latency_and_quality_only = 0.5 * 2.0;
    assert!((with_heavy_price - latency_and_quality_only - 0.256).abs() < 1e-12);
}

// ── Adaptive state ──────────────────────────────────────────────────────────

#[test]
fn backoff_after_k_rate_limits_is_two_to_the_k_minus_one_capped_at_64() {
    let mut m = ProviderMetrics::default();
    let now = m.backoff_at;
    for k in 1..=10u32 {
        m.record_rate_limit(now);
        let expected = ((1u64 << k) - 1).min(64) as f64;
        assert_eq!(m.backoff_factor, expected, "k={k}");
    }
}

#[test]
fn backoff_halves_per_success_from_cap() {
    let mut m = ProviderMetrics { backoff_factor: 64.0, ..Default::default() };
    let now = m.backoff_at;
    for j in 1..=10i32 {
        m.record_success(now);
        assert_eq!(m.backoff_factor, 64.0 / 2f64.powi(j));
    }
    assert!(m.backoff_factor < 0.1);
}

#[test]
fn backoff_stays_in_bounds_under_arbitrary_sequences() {
    let mut m = ProviderMetrics::default();
    let mut now = m.backoff_at;
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..100_000 {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        now += Duration::from_millis(x % 500);
        if x.is_multiple_of(3) { m.record_rate_limit(now) } else { m.record_success(now) }
        assert!((0.0..=64.0).contains(&m.backoff(now)));
    }
}

#[test]
fn backoff_halves_every_half_life_without_traffic() {
    let m = ProviderMetrics { backoff_factor: 64.0, ..Default::default() };
    let t0 = m.backoff_at;
    for k in 0..=6u32 {
        let t = t0 + Duration::from_secs_f64(BACKOFF_HALF_LIFE_SECS * k as f64);
        assert!(close(m.backoff(t), 64.0 / 2f64.powi(k as i32)), "k={k}");
    }
}

#[test]
fn error_rate_floor_limits_single_failure_to_one_fifth() {
    let mut m = ProviderMetrics::default();
    let now = m.backoff_at;
    m.push_error_sample(true, now);
    assert_eq!(m.error_rate(now), 1.0 / ERROR_MIN_WEIGHT);
    for n in 1..=4 {
        m.push_error_sample(false, now);
        assert_eq!(m.error_rate(now), 1.0 / ERROR_MIN_WEIGHT, "n={n}");
    }
    m.push_error_sample(false, now);
    assert_eq!(m.error_rate(now), 1.0 / 6.0);
}

#[test]
fn error_weight_halves_every_half_life() {
    let mut m = ProviderMetrics::default();
    let t0 = m.backoff_at;
    for _ in 0..10 {
        m.push_error_sample(true, t0);
    }
    for _ in 0..10 {
        m.push_error_sample(false, t0 + Duration::from_secs_f64(ERROR_HALF_LIFE_SECS));
    }
    let later = t0 + Duration::from_secs_f64(ERROR_HALF_LIFE_SECS);
    assert!(close(m.error_rate(later), 5.0 / 15.0));
}

fn demoted_pair(fault_age: Duration) -> (Registry, Vec<Arc<AtomicUsize>>) {
    // "free" had one rate-limit `fault_age` ago; "paid" costs 0.1024 more per Light call.
    let (r, calls, _) = Builder::new()
        .ok("free", ModelTier::Light, 0.0)
        .ok("paid", ModelTier::Light, 0.1)
        .build();
    let at = Instant::now() - fault_age;
    let mut m = r.slots[0].metrics.lock().unwrap();
    m.backoff_at = at;
    m.record_rate_limit(at);
    drop(m);
    (r, calls)
}

#[test]
fn demoted_provider_recovers_without_traffic() {
    let (r, calls) = demoted_pair(Duration::ZERO);
    r.try_complete(&req(ModelTier::Light, 0, 0)).unwrap();
    assert_eq!(calls[1].load(Ordering::SeqCst), 1);

    let (r, calls) = demoted_pair(Duration::from_secs(60));
    r.try_complete(&req(ModelTier::Light, 0, 0)).unwrap();
    assert_eq!(calls[0].load(Ordering::SeqCst), 1);
}

#[test]
fn recovery_time_grows_with_depth() {
    // After one rate-limit at t=0 the penalty is 2^{-t/Hb} + 2^d · (1/5) · 2^{-t/He}.
    // "free" wins again once that falls below the 0.1024 price gap.
    let recovered = |secs: u64, depth: u8| {
        let (r, calls) = demoted_pair(Duration::from_secs(secs));
        r.try_complete(&req(ModelTier::Light, 0, depth)).unwrap();
        calls[0].load(Ordering::SeqCst) == 1
    };
    let predicted = |secs: u64, depth: u8| {
        let t = secs as f64;
        let penalty = decay(Duration::from_secs_f64(t), BACKOFF_HALF_LIFE_SECS)
            + 2f64.powi(depth as i32) / ERROR_MIN_WEIGHT * decay(Duration::from_secs_f64(t), ERROR_HALF_LIFE_SECS);
        penalty < 0.1024
    };
    for depth in 0..=3u8 {
        for secs in (0..=240).step_by(5) {
            assert_eq!(recovered(secs, depth), predicted(secs, depth), "depth {depth} t={secs}s");
        }
    }
    assert!(recovered(60, 0) && !recovered(60, 3));
    assert!(recovered(180, 3));
}

#[test]
fn rolling_windows_hold_last_twenty_samples() {
    let mut m = ProviderMetrics::default();
    let now = m.backoff_at;
    assert_eq!(m.latency_p50_ms(), 2000.0);
    assert_eq!(m.error_rate(now), 0.0);
    for ms in 1..=100u64 {
        m.record_latency(ms);
        m.push_error_sample(ms <= 90, now);
    }
    assert_eq!(m.latency_samples.len(), 20);
    assert_eq!(m.latency_p50_ms(), 91.0);
    assert_eq!(m.error_rate(now), 0.5);
}

// ── Fallthrough and fan-out ─────────────────────────────────────────────────

#[test]
fn fallthrough_tries_in_score_order_until_success() {
    let (r, _, log) = Builder::new()
        .add("c-ok", vec![model(ModelTier::Heavy, 0.0, 2.0)], true, |_| reply("ok", 1))
        .add("a-429", vec![model(ModelTier::Heavy, 0.0, 0.0)], true, |_| Err(ProviderError::RateLimit))
        .add("b-err", vec![model(ModelTier::Heavy, 0.0, 1.0)], true, |_| {
            Err(ProviderError::Unavailable("down".into()))
        })
        .build();

    let resp = r.try_complete(&req(ModelTier::Heavy, 0, 0)).unwrap();
    assert_eq!(resp.text, "ok");
    assert_eq!(*log.lock().unwrap(), vec!["a-429", "b-err", "c-ok"]);

    let snap = r.snapshot();
    assert!((snap[1].backoff_factor - 1.0).abs() < 1e-3);
    assert!((snap[1].error_rate - 0.2).abs() < 1e-3);
    assert_eq!(snap[2].backoff_factor, 0.0);
    assert!((snap[2].error_rate - 0.2).abs() < 1e-3);
    assert_eq!(snap[0].error_rate, 0.0);
}

#[test]
fn fallthrough_succeeds_whenever_any_eligible_provider_succeeds() {
    for n in 1..=6usize {
        for good in 0..n {
            let mut b = Builder::new();
            for i in 0..n {
                let handler = move |_: &InferenceRequest| {
                    if i == good { reply("ok", 1) } else { Err(ProviderError::BadResponse("x".into())) }
                };
                b = b.add(&format!("p{i}"), vec![model(ModelTier::Heavy, 0.0, i as f64)], true, handler);
            }
            let (r, calls, _) = b.build();
            assert!(r.try_complete(&req(ModelTier::Heavy, 0, 0)).is_ok());
            for (i, c) in calls.iter().enumerate() {
                assert_eq!(c.load(Ordering::SeqCst), (i <= good) as usize, "n={n} good={good} i={i}");
            }
        }
    }
}

#[test]
fn all_failures_return_last_error() {
    let (r, _, _) = Builder::new()
        .add("a", vec![model(ModelTier::Heavy, 0.0, 0.0)], true, |_| Err(ProviderError::RateLimit))
        .add("b", vec![model(ModelTier::Heavy, 0.0, 1.0)], true, |_| Err(ProviderError::RateLimit))
        .build();
    assert!(matches!(r.try_complete(&req(ModelTier::Heavy, 0, 0)), Err(ProviderError::RateLimit)));
}

#[test]
fn slot_hint_rotation_gives_each_provider_first_position_once_per_cycle() {
    let n = 4usize;
    let mut firsts = Vec::new();
    for hint in 0..2 * n {
        let mut b = Builder::new();
        for i in 0..n {
            b = b.ok(&format!("p{i}"), ModelTier::Heavy, i as f64);
        }
        let (r, _, log) = b.build();
        let mut rq = req(ModelTier::Heavy, 0, 0);
        rq.slot_hint = Some(hint);
        r.try_complete(&rq).unwrap();
        firsts.push(log.lock().unwrap()[0].clone());
    }
    let expected: Vec<String> = (0..2 * n).map(|h| format!("p{}", h % n)).collect();
    assert_eq!(firsts, expected);
}

#[test]
fn capable_heavy_slots_counts_only_qualified_providers() {
    let (r, _, _) = Builder::new()
        .ok("heavy-good", ModelTier::Heavy, 0.0)
        .ok("heavy-low-quality", ModelTier::Heavy, 0.0)
        .ok("heavy-ctx-limited", ModelTier::Heavy, 0.0)
        .ok("light", ModelTier::Light, 0.0)
        .add("heavy-offline", vec![model(ModelTier::Heavy, 0.0, 0.0)], false, |_| reply("ok", 1))
        .build();
    r.slots[1].metrics.lock().unwrap().quality_score = 0.49;
    r.slots[2].metrics.lock().unwrap().context_ok = false;
    assert_eq!(r.capable_heavy_slots(), 1);
}

// ── Qualification queries ───────────────────────────────────────────────────

fn qq_answer(user: &str, truncate: bool) -> String {
    if user.contains("last number") {
        if truncate {
            return "1024".into();
        }
        let list = user.split("\n\n").next().unwrap();
        return list.rsplit(", ").next().unwrap().to_string();
    }
    if user.contains("days of the week") {
        return "Monday\nTuesday\nWednesday\nThursday\nFriday\nSaturday\nSunday".into();
    }
    if user.contains("CORRECT or INCORRECT") {
        return "INCORRECT\nIt subtracts.".into();
    }
    "use std::collections::HashMap;\nstruct M(HashMap<String,String>);\nimpl Store for M { fn get(&self, k: &str) -> Option<String> { self.0.get(k).cloned() } }".into()
}

fn qq_provider(truncate: bool) -> Builder {
    let latency = Arc::new(AtomicUsize::new(0));
    Builder::new().add("qq", vec![model(ModelTier::Heavy, 0.0, 0.0)], true, move |rq| {
        let ms = 100 * latency.fetch_add(1, Ordering::SeqCst) as u64;
        reply(&qq_answer(&rq.user, truncate), ms)
    })
}

#[test]
fn qq_perfect_provider_scores_one_with_median_latency() {
    let (r, _, _) = qq_provider(false).build();
    let out = crate::qq::probe(&*r.slots[0].provider);
    assert!(out.context_ok);
    assert!(!out.rate_limited);
    assert_eq!(out.quality_score, 1.0);
    // Context probe is call 0; quality probes run at 100, 200, 300 ms.
    assert_eq!(out.latency_p50_ms, 200);
}

#[test]
fn qq_detects_truncating_context_window() {
    let (r, _, _) = qq_provider(true).build();
    let out = crate::qq::probe(&*r.slots[0].provider);
    assert!(!out.context_ok);
    assert_eq!(out.quality_score, 1.0);
}

#[test]
fn qq_wrong_answers_score_zero_and_unparseable_verdict_scores_half() {
    let (r, _, _) = Builder::new()
        .add("wrong", vec![model(ModelTier::Heavy, 0.0, 0.0)], true, |rq| {
            if rq.user.contains("CORRECT or INCORRECT") { reply("CORRECT", 1) } else { reply("", 1) }
        })
        .add("hedge", vec![model(ModelTier::Heavy, 0.0, 0.0)], true, |rq| {
            if rq.user.contains("CORRECT or INCORRECT") { reply("It depends.", 1) } else { reply("", 1) }
        })
        .build();
    assert_eq!(crate::qq::probe(&*r.slots[0].provider).quality_score, 0.0);
    assert!((crate::qq::probe(&*r.slots[1].provider).quality_score - 0.5 / 3.0).abs() < 1e-12);
}

#[test]
fn qq_rate_limit_leaves_metrics_at_defaults() {
    let (r, calls, _) = Builder::new()
        .add("throttled", vec![model(ModelTier::Heavy, 0.0, 0.0)], true, |_| Err(ProviderError::RateLimit))
        .build();
    r.run_qq();
    assert_eq!(calls[0].load(Ordering::SeqCst), 1);
    let m = r.slots[0].metrics.lock().unwrap();
    assert!(m.context_ok);
    assert_eq!(m.quality_score, 1.0);
    assert!(m.latency_samples.is_empty());
}

#[test]
fn qq_non_rate_limit_failure_is_penalised() {
    let (r, _, _) = Builder::new()
        .add("broken", vec![model(ModelTier::Heavy, 0.0, 0.0)], true, |_| {
            Err(ProviderError::Unavailable("500".into()))
        })
        .build();
    r.run_qq();
    let m = r.slots[0].metrics.lock().unwrap();
    assert!(!m.context_ok);
    assert_eq!(m.quality_score, 0.0);
}
