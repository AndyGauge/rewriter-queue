use crate::{backends::Provider, types::{InferenceRequest, ModelTier}};

pub struct QqOutcome {
    pub context_ok: bool,
    pub quality_score: f64,
    pub latency_p50_ms: u64,
    /// True when the provider was rate-limited during QQ — metrics should keep their defaults.
    pub rate_limited: bool,
}

/// Run context probe + quality probes against a single provider.
/// Returns `rate_limited=true` when the provider is reachable but throttled —
/// callers should keep default metrics rather than penalising the provider.
pub fn probe(provider: &dyn Provider) -> QqOutcome {
    eprintln!("  [QQ] {} — context probe ...", provider.name());
    let (context_ok, rate_limited) = probe_context(provider);
    if rate_limited {
        eprintln!("  [QQ] {} — rate limited, keeping defaults", provider.name());
        return QqOutcome { context_ok: true, quality_score: 1.0, latency_p50_ms: 2000, rate_limited: true };
    }
    eprintln!(
        "  [QQ] {} — context: {}",
        provider.name(),
        if context_ok { "ok" } else { "limited (<4096 tokens)" }
    );

    let (quality, latency_p50_ms) = probe_quality(provider);
    eprintln!(
        "  [QQ] {} — quality: {:.2}  latency_p50: {}ms",
        provider.name(), quality, latency_p50_ms
    );

    QqOutcome { context_ok, quality_score: quality, latency_p50_ms, rate_limited: false }
}

/// Send ~4200 tokens of input. If the model correctly identifies the last number
/// in a padded list, it can see past the 4096-token boundary.
/// Returns `(context_ok, rate_limited)`.
fn probe_context(provider: &dyn Provider) -> (bool, bool) {
    const TARGET_CHARS: usize = 16_800;
    let mut filler = String::from("The following is a list of numbers: ");
    let mut last_n = 0u32;
    while filler.len() < TARGET_CHARS {
        last_n += 1;
        filler.push_str(&last_n.to_string());
        if filler.len() < TARGET_CHARS {
            filler.push_str(", ");
        }
    }
    filler.push_str(
        "\n\nWhat is the last number in the list above? Respond with only that number.",
    );

    let req = InferenceRequest::new(String::new(), filler, 16, ModelTier::Light, 0);

    match provider.complete(&req) {
        Ok(resp) => {
            let answer = resp.text.trim().to_string();
            let expected = last_n.to_string();
            (answer == expected || answer.contains(&expected), false)
        }
        Err(crate::types::ProviderError::RateLimit) => {
            eprintln!("  [QQ] {} context probe error: rate limit", provider.name());
            (false, true)
        }
        Err(e) => {
            eprintln!("  [QQ] {} context probe error: {e}", provider.name());
            (false, false)
        }
    }
}

/// Run three quality probes (Micro / Light / Heavy).
/// Returns (mean_quality 0.0–1.0, p50_latency_ms across all successful probes).
fn probe_quality(provider: &dyn Provider) -> (f64, u64) {
    let mut scores: Vec<f64> = Vec::new();
    let mut latencies: Vec<u64> = Vec::new();

    // ── Micro: list the days of the week ────────────────────────────────────
    let req = InferenceRequest::new(String::new(), "List the days of the week, one per line. No other text.".into(), 64, ModelTier::Micro, 0);
    if let Ok(resp) = provider.complete(&req) {
        latencies.push(resp.latency_ms);
        let text = resp.text.to_lowercase();
        let days = [
            "monday", "tuesday", "wednesday", "thursday",
            "friday", "saturday", "sunday",
        ];
        let found = days.iter().filter(|d| text.contains(*d)).count();
        scores.push(found as f64 / 7.0);
    }

    // ── Light: spot a bug in a Rust function ────────────────────────────────
    let req = InferenceRequest::new(String::new(), "Review this Rust function for correctness:\n\
               fn add(a: i32, b: i32) -> i32 { a - b }\n\
               Respond with exactly CORRECT or INCORRECT on the first line.".into(), 64, ModelTier::Light, 0);
    if let Ok(resp) = provider.complete(&req) {
        latencies.push(resp.latency_ms);
        let first = resp.text.trim().lines().next().unwrap_or("").to_uppercase();
        let score = if first.starts_with("INCORRECT") {
            1.0
        } else if first.starts_with("CORRECT") {
            0.0
        } else {
            0.5
        };
        scores.push(score);
    }

    // ── Heavy: implement a simple trait ─────────────────────────────────────
    let req = InferenceRequest::new(String::new(), "Given: trait Store { fn get(&self, key: &str) -> Option<String>; }\n\
               Write a HashMap-backed Rust implementation. Code only, no prose.".into(), 300, ModelTier::Heavy, 0);
    if let Ok(resp) = provider.complete(&req) {
        latencies.push(resp.latency_ms);
        let text = &resp.text;
        let checks = [
            text.contains("impl Store"),
            text.contains("HashMap"),
            text.contains("fn get"),
        ];
        let hit = checks.iter().filter(|&&b| b).count();
        scores.push(hit as f64 / checks.len() as f64);
    }

    let quality = if scores.is_empty() {
        0.0
    } else {
        scores.iter().sum::<f64>() / scores.len() as f64
    };

    let p50 = if latencies.is_empty() {
        2000
    } else {
        let mut sorted = latencies.clone();
        sorted.sort_unstable();
        sorted[sorted.len() / 2]
    };

    (quality, p50)
}
