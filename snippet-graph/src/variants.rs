use crate::bench::{compare, BenchConfig, Comparison};
use crate::fill::{build_prompt, extract_body, Generator};
use crate::model::{Graph, NodeId, NodeKind, TestReport};
use crate::version::{
    content_hash, BenchResult, NodeVersion, Origin, Results, RunId, Store, StoreError, VersionId,
    View,
};
use crate::view;
use std::collections::BTreeSet;
use std::time::{SystemTime, UNIX_EPOCH};

pub struct Evaluation {
    pub tests: TestReport,
    pub benches: Vec<BenchResult>,
}

pub struct Candidate {
    pub version: VersionId,
    pub passed: bool,
    pub bench: Option<BenchResult>,
}

pub struct TournamentReport {
    pub baseline: Option<BenchResult>,
    pub candidates: Vec<Candidate>,
    pub winner: Option<VersionId>,
    pub view: Option<View>,
}

/// Generate `n` variants of `node` in parallel (same signature), store each as a
/// version, evaluate each swapped into `base` (tests then benches), and pick the
/// fastest passing variant that beats the baseline beyond the noise threshold.
/// The winning view gets a composed evaluation before it is returned; a winner
/// that fails it is discarded. Every candidate is recorded in the store
/// whether it passes or not.
#[allow(clippy::too_many_arguments)]
pub fn tournament(
    store: &mut dyn Store,
    base: &View,
    node: &NodeId,
    n: usize,
    spec: &str,
    generators: &mut [Box<dyn Generator + Send>],
    evaluate: &mut dyn FnMut(&Graph) -> Evaluation,
    cfg: &BenchConfig,
    run: &RunId,
) -> Result<TournamentReport, StoreError> {
    let base_graph = view::to_graph(store, base)?;
    let base_version = base
        .versions
        .get(node)
        .cloned()
        .ok_or_else(|| StoreError::NotFound(node.0.clone()))?;
    let relevant = benches_reaching(&base_graph, node);
    let relevant_of = |benches: &[BenchResult]| -> Vec<BenchResult> {
        benches
            .iter()
            .filter(|b| relevant.contains(&b.bench))
            .cloned()
            .collect()
    };

    let baseline_eval = evaluate(&base_graph);
    record(store, base, &baseline_eval)?;
    let baseline_benches = relevant_of(&baseline_eval.benches);

    let prompt = variant_prompt(&base_graph, node, spec);
    let prompt_hash = content_hash(&[&prompt]);
    let responses = generate_all(generators, &prompt, n);

    let mut candidates = Vec::new();
    let mut views: Vec<(View, Vec<BenchResult>)> = Vec::new();
    for (i, response) in responses {
        let Some(body) = response.ok().as_deref().and_then(extract_body) else {
            continue;
        };
        let mut variant_node = base_graph.nodes[node].clone();
        variant_node.body = Some(body);
        let origin = Origin {
            run: run.clone(),
            model: format!("variant-{i}"),
            prompt_hash: prompt_hash.clone(),
            created_ms: now_ms(),
        };
        let version = store.put_version(NodeVersion::new(
            &variant_node,
            Some(base_version.clone()),
            origin,
        ))?;
        let view = view::swap(base, node, version.clone(), now_ms())?;
        store.put_view(view.clone())?;
        let graph = view::to_graph(store, &view)?;
        let eval = evaluate(&graph);
        record(store, &view, &eval)?;
        let passed = tests_passed(&eval.tests);
        let variant_benches = relevant_of(&eval.benches);
        candidates.push(Candidate {
            version,
            passed,
            bench: variant_benches.first().cloned(),
        });
        views.push((view, if passed { variant_benches } else { Vec::new() }));
    }

    let mut best: Option<(usize, f64)> = None;
    for (idx, (_, variant_benches)) in views.iter().enumerate() {
        if !candidates[idx].passed {
            continue;
        }
        if let Some(score) = beats_baseline(&baseline_benches, variant_benches, cfg.noise_pct) {
            if best.is_none_or(|(_, b)| score < b) {
                best = Some((idx, score));
            }
        }
    }

    let mut winner = None;
    let mut winning_view = None;
    if let Some((idx, _)) = best {
        let view = &views[idx].0;
        let graph = view::to_graph(store, view)?;
        let eval = evaluate(&graph);
        record(store, view, &eval)?;
        if tests_passed(&eval.tests) {
            winner = Some(candidates[idx].version.clone());
            winning_view = Some(view.clone());
        }
    }

    Ok(TournamentReport {
        baseline: baseline_benches.first().cloned(),
        candidates,
        winner,
        view: winning_view,
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn tests_passed(tests: &TestReport) -> bool {
    tests.compiled && tests.outcomes.iter().all(|o| o.passed)
}

fn record(store: &mut dyn Store, view: &View, eval: &Evaluation) -> Result<(), StoreError> {
    store.put_results(Results {
        view: view.id.clone(),
        tests: eval.tests.outcomes.clone(),
        benches: eval.benches.clone(),
        clippy_clean: None,
        recorded_ms: now_ms(),
    })
}

fn benches_reaching(graph: &Graph, node: &NodeId) -> BTreeSet<NodeId> {
    let mut reach: BTreeSet<NodeId> = BTreeSet::from([node.clone()]);
    loop {
        let before = reach.len();
        for (from, deps) in &graph.uses {
            if deps.iter().any(|d| reach.contains(d)) {
                reach.insert(from.clone());
            }
        }
        if reach.len() == before {
            break;
        }
    }
    reach
        .into_iter()
        .filter(|id| {
            graph
                .nodes
                .get(id)
                .is_some_and(|n| n.kind == NodeKind::Bench)
        })
        .collect()
}

/// The mean median ratio against the baseline when the variant is faster on at
/// least one bench and slower on none.
fn beats_baseline(
    baseline: &[BenchResult],
    variant: &[BenchResult],
    noise_pct: f64,
) -> Option<f64> {
    let mut faster = false;
    let mut ratios = Vec::new();
    for base in baseline {
        let var = variant.iter().find(|v| v.bench == base.bench)?;
        match compare(base, var, noise_pct) {
            Comparison::Slower => return None,
            Comparison::Faster => faster = true,
            Comparison::Same => {}
        }
        ratios.push(var.median_ns as f64 / (base.median_ns.max(1)) as f64);
    }
    if !faster {
        return None;
    }
    Some(ratios.iter().sum::<f64>() / ratios.len() as f64)
}

fn variant_prompt(graph: &Graph, node: &NodeId, spec: &str) -> String {
    let mut out = build_prompt(graph, node, &[]);
    if !spec.is_empty() {
        out.push_str(&format!("\nSpec:\n{spec}\n"));
    }
    if let Some(body) = graph.nodes.get(node).and_then(|n| n.body.as_deref()) {
        out.push_str(&format!(
            "\nCurrent implementation, which is the baseline to improve on for speed:\n```rust\n{body}\n```\n"
        ));
    }
    out.push_str(
        "\nWrite a faster implementation with the same signature. Behaviour must stay identical; every existing test must still pass.\n",
    );
    out
}

fn generate_all(
    generators: &mut [Box<dyn Generator + Send>],
    prompt: &str,
    n: usize,
) -> Vec<(usize, Result<String, String>)> {
    let len = generators.len();
    if len == 0 {
        return Vec::new();
    }
    let mut out: Vec<(usize, Result<String, String>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = generators
            .iter_mut()
            .enumerate()
            .map(|(g, generator)| {
                scope.spawn(move || {
                    (g..n)
                        .step_by(len)
                        .map(|i| (i, generator.generate(prompt)))
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        handles
            .into_iter()
            .flat_map(|h| h.join().unwrap_or_default())
            .collect()
    });
    out.sort_by_key(|(i, _)| *i);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Node, Placement, TestOutcome};
    use crate::version::MemoryStore;

    struct Fake(Vec<String>, usize);

    impl Generator for Fake {
        fn generate(&mut self, _prompt: &str) -> Result<String, String> {
            let r = self.0[self.1 % self.0.len()].clone();
            self.1 += 1;
            Ok(r)
        }
    }

    fn node(id: &str, kind: NodeKind, sig: &str, body: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            kind,
            signature: sig.into(),
            body: Some(body.into()),
            impl_header: None,
            placement: Placement {
                file: "lib.rs".into(),
                index: 0,
            },
        }
    }

    fn setup() -> (MemoryStore, View, RunId) {
        let mut g = Graph::default();
        g.insert(node(
            "f",
            NodeKind::Function,
            "pub fn f() -> u32",
            "{ 100 }",
        ));
        g.insert(node("t", NodeKind::Test, "#[test] fn t()", "{}"));
        g.insert(node(
            "bench_f",
            NodeKind::Bench,
            "#[test] fn bench_f()",
            "{}",
        ));
        g.add_use(&NodeId("t".into()), &NodeId("f".into()));
        g.add_use(&NodeId("bench_f".into()), &NodeId("f".into()));
        let run = RunId("r".into());
        let origin = Origin {
            run: run.clone(),
            model: "base".into(),
            prompt_hash: String::new(),
            created_ms: 0,
        };
        let mut store = MemoryStore::default();
        let view = view::from_graph(&mut store, &g, &run, &origin, None).unwrap();
        store.put_view(view.clone()).unwrap();
        (store, view, run)
    }

    fn gens(bodies: &[&str]) -> Vec<Box<dyn Generator + Send>> {
        vec![Box::new(Fake(
            bodies.iter().map(|b| b.to_string()).collect(),
            0,
        ))]
    }

    fn eval_fn(composed_fails: bool) -> impl FnMut(&Graph) -> Evaluation {
        let mut seen_fast = 0;
        move |g: &Graph| {
            let body = g.nodes[&NodeId("f".into())].body.clone().unwrap();
            let (compiled, passed, median) = match body.as_str() {
                "{ 100 }" => (true, true, 1000),
                "{ 1 }" => (true, true, 100),
                "{ 2 }" => (true, true, 500),
                "{ 3 }" => (true, true, 980),
                "{ 4 }" => (true, false, 10),
                _ => (false, false, 0),
            };
            let passed = if body == "{ 1 }" && composed_fails {
                seen_fast += 1;
                seen_fast < 2
            } else {
                passed
            };
            Evaluation {
                tests: TestReport {
                    compiled,
                    outcomes: if compiled {
                        vec![TestOutcome {
                            test: NodeId("t".into()),
                            passed,
                            message: String::new(),
                        }]
                    } else {
                        Vec::new()
                    },
                    ..TestReport::default()
                },
                benches: if compiled {
                    vec![BenchResult {
                        bench: NodeId("bench_f".into()),
                        median_ns: median,
                        spread_ns: 1,
                        runs: 5,
                        instructions: None,
                    }]
                } else {
                    Vec::new()
                },
            }
        }
    }

    fn cfg() -> BenchConfig {
        BenchConfig {
            runs: 5,
            noise_pct: 5.0,
        }
    }

    fn run_with(bodies: &[&str], composed_fails: bool) -> (MemoryStore, TournamentReport) {
        let (mut store, view, run) = setup();
        let mut g = gens(bodies);
        let mut ev = eval_fn(composed_fails);
        let report = tournament(
            &mut store,
            &view,
            &NodeId("f".into()),
            bodies.len(),
            "spec",
            &mut g,
            &mut ev,
            &cfg(),
            &run,
        )
        .unwrap();
        (store, report)
    }

    #[test]
    fn fastest_passing_wins_and_failures_are_excluded() {
        let (store, report) = run_with(&["{ 2 }", "{ 4 }", "{ 1 }", "not code"], false);
        assert_eq!(report.candidates.len(), 3);
        assert_eq!(report.candidates.iter().filter(|c| c.passed).count(), 2);
        let winner = report.winner.clone().unwrap();
        let v = store.get_version(&winner).unwrap();
        assert_eq!(v.body.as_deref(), Some("{ 1 }"));
        assert_eq!(report.view.unwrap().versions[&NodeId("f".into())], winner);
        assert_eq!(report.baseline.unwrap().median_ns, 1000);
    }

    #[test]
    fn variant_within_noise_does_not_displace_baseline() {
        let (_, report) = run_with(&["{ 3 }"], false);
        assert_eq!(report.candidates.len(), 1);
        assert!(report.winner.is_none());
        assert!(report.view.is_none());
    }

    #[test]
    fn composed_check_failure_discards_winner() {
        let (_, report) = run_with(&["{ 1 }"], true);
        assert!(report.candidates[0].passed);
        assert!(report.winner.is_none());
        assert!(report.view.is_none());
    }

    #[test]
    fn every_candidate_is_stored_with_baseline_parent() {
        let (store, report) = run_with(&["{ 2 }", "{ 4 }", "{ 99 }"], false);
        let versions = store.versions_of(&NodeId("f".into())).unwrap();
        assert_eq!(versions.len(), 4);
        let base = versions[0].clone();
        for c in &report.candidates {
            assert!(versions.contains(&c.version));
            assert_eq!(
                store.get_version(&c.version).unwrap().parent,
                Some(base.clone())
            );
        }
        assert_eq!(store.all_results().unwrap().len(), 4);
    }
}
