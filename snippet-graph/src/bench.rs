use crate::assemble::Assembled;
use crate::model::NodeId;
use crate::version::BenchResult;
use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

#[derive(Clone, Debug)]
pub struct BenchConfig {
    pub runs: u32,
    pub noise_pct: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Comparison {
    Faster,
    Slower,
    Same,
}

/// Write the files, build in release mode, and run each bench node (all when
/// `only` is empty) `cfg.runs` times, reporting median and spread. A bench node
/// is a `#[test]`-attributed function that prints `BENCH <ns>` after timing its
/// workload.
pub fn run_benches(
    crate_dir: &Path,
    assembled: &Assembled,
    only: &[NodeId],
    cfg: &BenchConfig,
) -> std::io::Result<Vec<BenchResult>> {
    for (path, text) in &assembled.files {
        let full = crate_dir.join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(full, text)?;
    }

    let mut names = bench_names(assembled);
    if !only.is_empty() {
        names.retain(|id, _| only.contains(id));
    }
    if names.is_empty() {
        return Ok(Vec::new());
    }

    let build = cargo(crate_dir, &["test", "--release", "--no-run"])?;
    if !build.status.success() {
        return Err(std::io::Error::other(
            String::from_utf8_lossy(&build.stderr).into_owned(),
        ));
    }

    let listing = cargo(crate_dir, &["test", "--release", "--", "--list"])?;
    let listed: Vec<String> = String::from_utf8_lossy(&listing.stdout)
        .lines()
        .filter_map(|l| l.strip_suffix(": test"))
        .map(str::to_string)
        .collect();

    let mut results = Vec::new();
    for (id, name) in names {
        let Some(path) = listed
            .iter()
            .find(|p| p.rsplit("::").next() == Some(name.as_str()))
        else {
            continue;
        };
        let mut samples = Vec::new();
        for _ in 0..cfg.runs {
            let out = cargo(
                crate_dir,
                &["test", "--release", "--", "--exact", path, "--nocapture"],
            )?;
            if let Some(ns) = parse_bench(&String::from_utf8_lossy(&out.stdout)) {
                samples.push(ns);
            }
        }
        if samples.is_empty() {
            continue;
        }
        samples.sort_unstable();
        let n = samples.len();
        let median_ns = if n % 2 == 1 {
            samples[n / 2]
        } else {
            (samples[n / 2 - 1] + samples[n / 2]) / 2
        };
        results.push(BenchResult {
            bench: id,
            median_ns,
            spread_ns: samples[n - 1] - samples[0],
            runs: cfg.runs,
            instructions: None,
        });
    }
    Ok(results)
}

fn cargo(dir: &Path, args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("cargo")
        .args(args)
        .current_dir(dir)
        .env_remove("CARGO_TARGET_DIR")
        .stdin(Stdio::null())
        .output()
}

fn parse_bench(stdout: &str) -> Option<u64> {
    stdout
        .lines()
        .filter_map(|l| l.trim().strip_prefix("BENCH "))
        .filter_map(|v| v.trim().parse::<u64>().ok())
        .next_back()
}

fn fn_name(snippet: &str) -> Option<&str> {
    let rest = snippet.split("fn ").nth(1)?.trim_start();
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
}

fn bench_names(assembled: &Assembled) -> BTreeMap<NodeId, String> {
    let mut names = BTreeMap::new();
    for (id, span) in &assembled.spans {
        let Some(text) = assembled.files.get(&span.file) else {
            continue;
        };
        let snippet: Vec<&str> = text
            .lines()
            .skip(span.start_line.saturating_sub(1))
            .take((span.end_line + 1).saturating_sub(span.start_line))
            .collect();
        let snippet = snippet.join("\n");
        let Some(name) = fn_name(&snippet) else {
            continue;
        };
        if snippet.contains("BENCH") || name.starts_with("bench_") {
            names.insert(id.clone(), name.to_string());
        }
    }
    names
}

/// `Faster` only when the variant's median is below the baseline's by more than
/// `noise_pct` percent and more than both spreads; `Slower` symmetrically.
pub fn compare(baseline: &BenchResult, variant: &BenchResult, noise_pct: f64) -> Comparison {
    let base = baseline.median_ns as f64;
    let var = variant.median_ns as f64;
    let delta = (base - var).abs();
    let threshold = base * noise_pct / 100.0;
    let spread = baseline.spread_ns.max(variant.spread_ns) as f64;
    if delta <= threshold || delta <= spread {
        Comparison::Same
    } else if var < base {
        Comparison::Faster
    } else {
        Comparison::Slower
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assemble::Span;
    use std::path::PathBuf;

    fn result(median: u64, spread: u64) -> BenchResult {
        BenchResult {
            bench: NodeId("b".into()),
            median_ns: median,
            spread_ns: spread,
            runs: 5,
            instructions: None,
        }
    }

    #[test]
    fn against_itself_is_same() {
        let r = result(1000, 10);
        assert_eq!(compare(&r, &r, 5.0), Comparison::Same);
    }

    #[test]
    fn clear_win_and_loss() {
        let base = result(1000, 10);
        assert_eq!(compare(&base, &result(500, 10), 5.0), Comparison::Faster);
        assert_eq!(compare(&base, &result(2000, 10), 5.0), Comparison::Slower);
    }

    #[test]
    fn within_spread_or_noise_is_same() {
        let base = result(1000, 300);
        assert_eq!(compare(&base, &result(800, 10), 5.0), Comparison::Same);
        assert_eq!(compare(&base, &result(1200, 10), 5.0), Comparison::Same);
        assert_eq!(
            compare(&result(1000, 1), &result(980, 1), 5.0),
            Comparison::Same
        );
    }

    #[test]
    fn parses_last_bench_line() {
        assert_eq!(parse_bench("x\nBENCH 12\nBENCH 34\n"), Some(34));
        assert_eq!(parse_bench("nothing"), None);
    }

    #[test]
    fn slow_bench_reports_slower_than_fast() {
        let dir = std::env::temp_dir().join(format!("sg_bench_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"tiny\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
        )
        .unwrap();
        let src = r#"#[cfg(test)]
mod tests {
    use std::hint::black_box;
    use std::time::Instant;

    #[test]
    fn bench_fast() {
        let t = Instant::now();
        let mut s = 0u64;
        for i in 0..1_000u64 { s = s.wrapping_add(black_box(i)); }
        black_box(s);
        println!("BENCH {}", t.elapsed().as_nanos());
    }

    #[test]
    fn bench_slow() {
        let t = Instant::now();
        let mut s = 0u64;
        for i in 0..100_000_000u64 { s = s.wrapping_add(black_box(i)); }
        black_box(s);
        println!("BENCH {}", t.elapsed().as_nanos());
    }
}
"#;
        let file = PathBuf::from("src/lib.rs");
        let assembled = Assembled {
            files: BTreeMap::from([(file.clone(), src.to_string())]),
            spans: BTreeMap::from([
                (
                    NodeId("fast".into()),
                    Span {
                        file: file.clone(),
                        start_line: 6,
                        end_line: 12,
                    },
                ),
                (
                    NodeId("slow".into()),
                    Span {
                        file: file.clone(),
                        start_line: 14,
                        end_line: 20,
                    },
                ),
            ]),
        };
        let cfg = BenchConfig {
            runs: 3,
            noise_pct: 5.0,
        };
        let results = run_benches(&dir, &assembled, &[], &cfg).unwrap();
        let get = |n: &str| results.iter().find(|r| r.bench.0 == n).unwrap().clone();
        let (fast, slow) = (get("fast"), get("slow"));
        assert!(slow.median_ns > fast.median_ns);
        assert_eq!(compare(&fast, &slow, cfg.noise_pct), Comparison::Slower);
        assert_eq!(compare(&slow, &fast, cfg.noise_pct), Comparison::Faster);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
