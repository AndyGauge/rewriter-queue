use inference_providers::config::Config;
use inference_providers::{InferenceRequest, ModelTier, ReasoningEffort, Registry};
use snippet_graph::assemble::assemble;
use snippet_graph::bench::{self, BenchConfig};
use snippet_graph::benchgen;
use snippet_graph::blame;
use snippet_graph::cli;
use snippet_graph::fill::{fill_all_versioned, Generator};
use snippet_graph::handoff;
use snippet_graph::structure;
use snippet_graph::import::write_back;
use snippet_graph::ledger;
use snippet_graph::lintfix;
use snippet_graph::metrics::RunMetrics;
use snippet_graph::review;
use snippet_graph::parallel::fill_parallel_versioned;
use snippet_graph::skeleton;
use snippet_graph::store::DirStore;
use snippet_graph::testgen;
use snippet_graph::variants::{self, Evaluation};
use snippet_graph::verify;
use snippet_graph::version::{Origin, Results, RunId, Store, View};
use snippet_graph::view;
use snippet_graph::{Graph, NodeId, NodeKind, TestReport};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const SKELETON_ATTEMPTS: usize = 4;
const FILL_RETRIES: usize = 3;
const TESTGEN_RETRIES: usize = 3;
const REPAIR_ROUNDS: usize = 4;
const LINT_PRUNE_ROUNDS: usize = 3;
const MAX_REPAIRS_PER_ROUND: usize = 4;
const DEFAULT_WIDTH: usize = 4;
const NODE_ATTEMPT_BUDGET: usize = 10;
const MAX_TOKENS: u32 = 16000;
const BENCH_RETRIES: usize = 3;
const MODEL: &str = "sg";
const USAGE: &str = "usage:
  sg [run] <spec.md> <crate_dir> [--oracle file.rs] [--width N] [--resume sg-state.json] [--prelude file.rs] [--store DIR] [--handoff DIR] [--no-review]
  sg history <store> [--node ID]
  sg diff <store> <a> <b>
  sg failures <store> [--node ID] [--open]
  sg good <store> <node>
  sg sweep <store> <view> [--dry-run]
  sg undo <store> <view>
  sg swap <store> <view> <node> <version>
  sg export <store> <view> <dir> [--commit]
  sg ab <store> <view> <node> --spec FILE --crate DIR [--n 4] [--runs 7] [--noise 5] [--width N]
  sg edit <store> <view> <crate_dir> --spec FILE <node ids...>";

#[derive(Default)]
struct Usage {
    input: AtomicU64,
    output: AtomicU64,
    calls: AtomicU64,
}

struct RegistryGenerator {
    registry: Arc<Registry>,
    slot: Option<usize>,
    usage: Arc<Usage>,
}

const SYSTEM: &str = "You are a precise Rust engineer. Output only what is asked, no commentary.";

impl Generator for RegistryGenerator {
    fn generate(&mut self, prompt: &str) -> Result<String, String> {
        let mut req = InferenceRequest::new(
            SYSTEM.into(),
            prompt.into(),
            MAX_TOKENS,
            ModelTier::Heavy,
            1,
        );
        req.slot_hint = self.slot;
        req.reasoning_effort = match std::env::var("SG_REASONING_EFFORT").as_deref() {
            Ok("low") => Some(ReasoningEffort::Low),
            Ok("medium") => Some(ReasoningEffort::Medium),
            Ok("high") => Some(ReasoningEffort::High),
            _ => None,
        };
        let started = Instant::now();
        let response = self.registry.complete(&req).map_err(|e| e.to_string())?;
        self.usage
            .input
            .fetch_add(u64::from(response.input_tokens), Ordering::Relaxed);
        self.usage
            .output
            .fetch_add(u64::from(response.output_tokens), Ordering::Relaxed);
        let n = self.usage.calls.fetch_add(1, Ordering::Relaxed) + 1;
        eprintln!(
            "  model call {n}: {}s, {}+{} tokens",
            started.elapsed().as_secs(),
            response.input_tokens,
            response.output_tokens
        );
        Ok(response.text)
    }
}

fn strip_fences(text: &str) -> String {
    review::unfence(text)
}

fn skeleton_prompt(spec: &str, prelude: &str, errors: &[String], previous: &str) -> String {
    let mut p = format!(
        "Write the Rust skeleton for this spec as a single lib.rs: all types, consts, use items, and function/method signatures, with every function body exactly `{{ todo!() }}`. No logic. Private helpers that need no state must be free functions, not methods taking `&self`; never add a function whose only purpose is to call other functions. Keep each function to one job: if its contract would need more than three separate duties (filtering, ordering, several aggregates, exclusions), split the steps into small private helper functions that it calls. Put a `///` doc comment on every new type and function stating its contract for callers who cannot see the body: what it returns, which inputs it skips or treats specially, and any rounding, ordering or error behaviour from the spec. Output only Rust source.\n\nSpec:\n{spec}\n"
    );
    if !prelude.is_empty() {
        p.push_str(&format!(
            "\nThe file already begins with these items, which are finished. Do not repeat or change them; write only the new items that follow them:\n```rust\n{prelude}\n```\n"
        ));
    }
    if !errors.is_empty() {
        p.push_str(&format!(
            "\nYour previous skeleton:\n{previous}\n\nIt failed to compile:\n{}\nFix it.\n",
            errors.join("\n")
        ));
    }
    p
}

fn check_graph(crate_dir: &Path, graph: &Graph) -> std::io::Result<verify::Report> {
    verify::check(crate_dir, &assemble(graph))
}

const PRELUDE_GAP: usize = 10_000;

fn with_prelude(prelude: &Graph, mut model: Graph) -> Graph {
    model.nodes.retain(|id, _| !prelude.nodes.contains_key(id));
    for node in model.nodes.values_mut() {
        node.placement.index += PRELUDE_GAP;
    }
    let mut combined = prelude.clone();
    skeleton::merge(&mut combined, model);
    combined.frozen = prelude.nodes.keys().cloned().collect();
    combined
}

fn build_skeleton(
    crate_dir: &Path,
    text: &str,
    prelude: Option<&Graph>,
) -> Result<Graph, Vec<String>> {
    let mut graph = skeleton::parse(Path::new("src/lib.rs"), text).map_err(|e| vec![e.to_string()])?;
    if let Some(prelude) = prelude {
        graph = with_prelude(prelude, graph);
    }
    let report = check_graph(crate_dir, &graph).map_err(|e| vec![e.to_string()])?;
    if report.ok {
        Ok(graph)
    } else {
        Err(report
            .by_node
            .into_values()
            .flatten()
            .chain(report.unattributed)
            .collect())
    }
}

fn make_skeleton(
    gen: &mut RegistryGenerator,
    crate_dir: &Path,
    spec: &str,
    prelude: Option<&str>,
    review_rounds: usize,
) -> Result<(Graph, Vec<String>), String> {
    let prelude_text = prelude.unwrap_or_default();
    let prelude_graph = match prelude {
        Some(text) => Some(
            skeleton::parse(Path::new("src/lib.rs"), text).map_err(|e| e.to_string())?,
        ),
        None => None,
    };
    let mut errors = Vec::new();
    let mut previous = String::new();
    for attempt in 1..=SKELETON_ATTEMPTS {
        eprintln!("skeleton attempt {attempt}");
        previous = strip_fences(&gen.generate(&skeleton_prompt(
            spec,
            prelude_text,
            &errors,
            &previous,
        ))?);
        match build_skeleton(crate_dir, &previous, prelude_graph.as_ref()) {
            Ok(graph) => {
                let reviewed = review::review_skeleton(
                    graph,
                    previous,
                    spec,
                    gen,
                    &mut |text| build_skeleton(crate_dir, text, prelude_graph.as_ref()),
                    review_rounds,
                );
                for note in &reviewed.notes {
                    eprintln!("skeleton review: {note}");
                }
                return Ok((reviewed.graph, reviewed.notes));
            }
            Err(e) => errors = e,
        }
    }
    Err(format!(
        "no compiling skeleton after {SKELETON_ATTEMPTS} attempts"
    ))
}

struct Args {
    spec: PathBuf,
    crate_dir: PathBuf,
    oracle: Option<PathBuf>,
    resume: Option<PathBuf>,
    prelude: Option<PathBuf>,
    handoff: Option<PathBuf>,
    store: Option<PathBuf>,
    width: usize,
    review: bool,
}

struct Flags {
    positional: Vec<String>,
    values: BTreeMap<String, String>,
    switches: Vec<String>,
}

impl Flags {
    fn parse(args: Vec<String>, valued: &[&str], switches: &[&str]) -> Result<Flags, String> {
        let mut flags = Flags {
            positional: Vec::new(),
            values: BTreeMap::new(),
            switches: Vec::new(),
        };
        let mut it = args.into_iter();
        while let Some(a) = it.next() {
            if valued.contains(&a.as_str()) {
                let v = it.next().ok_or_else(|| format!("{a} needs a value"))?;
                flags.values.insert(a, v);
            } else if switches.contains(&a.as_str()) {
                flags.switches.push(a);
            } else if a.starts_with("--") {
                return Err(format!("unknown flag {a}"));
            } else {
                flags.positional.push(a);
            }
        }
        Ok(flags)
    }

    fn has(&self, name: &str) -> bool {
        self.switches.iter().any(|s| s == name)
    }

    fn path(&self, name: &str) -> Option<PathBuf> {
        self.values.get(name).map(PathBuf::from)
    }

    fn number<T: std::str::FromStr>(&self, name: &str, default: T) -> Result<T, String> {
        match self.values.get(name) {
            Some(v) => v.parse().map_err(|_| format!("{name}: bad number {v}")),
            None => Ok(default),
        }
    }

    fn required(&self, name: &str) -> Result<PathBuf, String> {
        self.path(name).ok_or_else(|| format!("{name} is required"))
    }
}

fn parse_args(args: Vec<String>) -> Result<Args, String> {
    let flags = Flags::parse(
        args,
        &["--oracle", "--resume", "--width", "--store", "--prelude", "--handoff"],
        &["--no-review"],
    )?;
    let width = flags.number("--width", DEFAULT_WIDTH)?;
    let [spec, dir] = <[String; 2]>::try_from(flags.positional.clone())
        .map_err(|_| "expected <spec.md> <crate_dir>".to_string())?;
    Ok(Args {
        spec: spec.into(),
        crate_dir: dir.into(),
        oracle: flags.path("--oracle"),
        resume: flags.path("--resume"),
        prelude: flags.path("--prelude"),
        handoff: flags.path("--handoff"),
        store: flags.path("--store"),
        width,
        review: !flags.has("--no-review"),
    })
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn io(e: impl ToString) -> String {
    e.to_string()
}

fn origin(run: &RunId) -> Origin {
    Origin {
        run: run.clone(),
        model: MODEL.into(),
        prompt_hash: String::new(),
        created_ms: now_ms(),
    }
}

fn record_view(
    store: &mut dyn Store,
    graph: &Graph,
    run: &RunId,
    parent: Option<&View>,
) -> Result<View, String> {
    let v = view::from_graph(store, graph, run, &origin(run), parent).map_err(io)?;
    store.put_view(v.clone()).map_err(io)?;
    Ok(v)
}

fn open_store(dir: &str) -> Result<DirStore, String> {
    DirStore::open(Path::new(dir)).map_err(io)
}

fn new_run_id() -> RunId {
    RunId(format!("run-{}", now_ms()))
}

fn generator_pool(
    registry: &Arc<Registry>,
    usage: &Arc<Usage>,
    width: usize,
) -> Vec<Box<dyn Generator + Send>> {
    (0..width.max(1))
        .map(|i| {
            Box::new(RegistryGenerator {
                registry: registry.clone(),
                slot: Some(i),
                usage: usage.clone(),
            }) as Box<dyn Generator + Send>
        })
        .collect()
}

fn registry_generator(registry: &Arc<Registry>, usage: &Arc<Usage>) -> RegistryGenerator {
    RegistryGenerator {
        registry: registry.clone(),
        slot: None,
        usage: usage.clone(),
    }
}

fn save_state(crate_dir: &Path, graph: &Graph) -> Result<(), String> {
    let text = serde_json::to_string(graph).map_err(|e| e.to_string())?;
    std::fs::write(crate_dir.join("sg-state.json"), text).map_err(|e| e.to_string())
}

fn load_state(path: &Path) -> Result<Graph, String> {
    let text = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
    let mut graph: Graph = serde_json::from_str(&text).map_err(|e| e.to_string())?;
    for node in graph.nodes.values_mut() {
        if matches!(node.kind, NodeKind::Function | NodeKind::Method) {
            node.body = None;
        }
    }
    Ok(graph)
}

fn all_tests(crate_dir: &Path, graph: &Graph) -> TestReport {
    verify::run_tests(crate_dir, &assemble(graph), &[]).unwrap_or_default()
}

fn failing(report: &TestReport) -> usize {
    report.outcomes.iter().filter(|o| !o.passed).count()
}

fn run_oracle(crate_dir: &Path, graph: &Graph, oracle: &Path) -> Result<bool, String> {
    let work = std::env::temp_dir().join(format!("sg-oracle-{}", std::process::id()));
    std::fs::create_dir_all(work.join("src")).map_err(|e| e.to_string())?;
    std::fs::copy(crate_dir.join("Cargo.toml"), work.join("Cargo.toml"))
        .map_err(|e| e.to_string())?;
    let lib = assemble(graph)
        .files
        .into_values()
        .next()
        .unwrap_or_default();
    let oracle_src = std::fs::read_to_string(oracle).map_err(|e| e.to_string())?;
    std::fs::write(work.join("src/lib.rs"), format!("{lib}\n{oracle_src}"))
        .map_err(|e| e.to_string())?;
    let output = Command::new("cargo")
        .args(["test", "--quiet"])
        .current_dir(&work)
        .output()
        .map_err(|e| e.to_string())?;
    eprint!("{}", String::from_utf8_lossy(&output.stdout));
    eprint!("{}", String::from_utf8_lossy(&output.stderr));
    Ok(output.status.success())
}

fn run(args: Args) -> Result<bool, String> {
    let started = Instant::now();
    let spec = std::fs::read_to_string(&args.spec).map_err(|e| e.to_string())?;
    let registry = Arc::new(Registry::from_config(&Config::load()));
    let usage = Arc::new(Usage::default());
    let mut gen = registry_generator(&registry, &usage);
    let crate_dir = args.crate_dir;
    let store_dir = args.store.unwrap_or_else(|| crate_dir.join(".sg-store"));
    let mut store = DirStore::open(&store_dir).map_err(io)?;
    let run_id = new_run_id();

    let prelude_text = match &args.prelude {
        Some(path) => Some(std::fs::read_to_string(path).map_err(|e| e.to_string())?),
        None => None,
    };
    let (mut graph, untested, review_notes) = match &args.resume {
        Some(path) => {
            let mut graph = load_state(path)?;
            graph.spec = spec.clone();
            (graph, Vec::new(), Vec::new())
        }
        None => {
            let rounds = if args.review { review::MAX_REVIEW_ROUNDS } else { 0 };
            let (mut graph, review_notes) =
                make_skeleton(&mut gen, &crate_dir, &spec, prelude_text.as_deref(), rounds)?;
            graph.spec = spec.clone();
            eprintln!("skeleton ok: {} nodes", graph.nodes.len());
            let mut check_tests = |g: &Graph| all_tests(&crate_dir, g);
            let untested = testgen::generate_all(
                &mut graph,
                &spec,
                &mut gen,
                &mut check_tests,
                TESTGEN_RETRIES,
            );
            for (id, why) in &untested {
                eprintln!("  no tests for {}: {why}", id.0);
            }
            save_state(&crate_dir, &graph)?;
            (graph, untested, review_notes)
        }
    };
    let test_count = graph
        .nodes
        .values()
        .filter(|n| n.kind == NodeKind::Test)
        .count();
    eprintln!("tests ok: {test_count} test nodes");

    let first = record_view(&mut store, &graph, &run_id, None)?;
    let mut generators = generator_pool(&registry, &usage, args.width);
    let lint = |g: &Graph| -> BTreeMap<NodeId, Vec<String>> {
        let mut assembled = assemble(g);
        for text in assembled.files.values_mut() {
            text.insert_str(0, "#![allow(dead_code)] ");
        }
        verify::clippy(&crate_dir, &assembled)
            .map(|r| r.by_node)
            .unwrap_or_default()
    };
    graph.budget = NODE_ATTEMPT_BUDGET;
    let (mut outcome, mut current) = fill_parallel_versioned(
        &mut graph,
        &mut generators,
        &lint,
        FILL_RETRIES,
        &mut store,
        &run_id,
        MODEL,
        Some(&first),
    )
    .map_err(io)?;
    eprintln!(
        "filled {}, failed {}",
        outcome.filled.len(),
        outcome.failed.len()
    );

    for round in 1..=REPAIR_ROUNDS {
        let report = all_tests(&crate_dir, &graph);
        if report.compiled && failing(&report) == 0 {
            break;
        }
        let suspects = blame::rank(&graph, &report.outcomes, &[]);
        eprintln!(
            "repair round {round}: {} failing tests, {} suspects",
            failing(&report),
            suspects.len()
        );
        let targets = blame::repair_targets(&graph, &suspects, MAX_REPAIRS_PER_ROUND);
        if targets.is_empty() {
            break;
        }
        eprintln!(
            "  retrying {}",
            targets
                .iter()
                .map(|t| t.0.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
        let saved: Vec<(NodeId, String)> = graph
            .nodes
            .iter()
            .filter(|(_, n)| matches!(n.kind, NodeKind::Function | NodeKind::Method))
            .filter_map(|(id, n)| n.body.clone().map(|b| (id.clone(), b)))
            .collect();
        for target in &targets {
            let mut findings = graph
                .nodes
                .get(target)
                .and_then(|n| n.body.as_deref())
                .map(|b| structure::evaluate_body(target, b))
                .unwrap_or_default();
            findings.extend(structure::test_load(&graph, target));
            match structure::refactor_reason(&findings) {
                Some(reason) => {
                    eprintln!("  {} judged too large: {reason}", target.0);
                    graph.refactor.insert(target.clone(), reason);
                }
                None => {
                    graph.refactor.remove(target);
                }
            }
            if let Some(node) = graph.nodes.get_mut(target) {
                node.body = None;
            }
        }
        let mut run_tests = |g: &Graph, only: &[NodeId]| {
            verify::run_tests(&crate_dir, &assemble(g), only).unwrap_or_default()
        };
        let (mut again, view) = fill_all_versioned(
            &mut graph,
            &mut gen,
            &mut run_tests,
            FILL_RETRIES,
            &mut store,
            &run_id,
            MODEL,
            Some(&current),
        )
        .map_err(io)?;
        current = view;
        if !again.failed.is_empty() {
            for (id, why) in &again.failed {
                eprintln!("  revising tests for {}", id.0);
                let mut check_tests = |g: &Graph| all_tests(&crate_dir, g);
                let _ = testgen::revise_tests(
                    &mut graph,
                    &spec,
                    id,
                    std::slice::from_ref(why),
                    &mut gen,
                    &mut check_tests,
                    TESTGEN_RETRIES,
                );
            }
            let (second, view) = fill_all_versioned(
                &mut graph,
                &mut gen,
                &mut run_tests,
                FILL_RETRIES,
                &mut store,
                &run_id,
                MODEL,
                Some(&current),
            )
            .map_err(io)?;
            current = view;
            again.filled.extend(second.filled);
            again.failed = second.failed;
            for (id, n) in second.retries {
                *again.retries.entry(id).or_default() += n;
            }
            for (id, n) in second.rollbacks {
                *again.rollbacks.entry(id).or_default() += n;
            }
        }
        for (id, body) in saved {
            if let Some(node) = graph.nodes.get_mut(&id) {
                if node.body.is_none() {
                    node.body = Some(body);
                }
            }
        }
        outcome.filled.extend(again.filled);
        outcome.failed.extend(again.failed);
        for (id, n) in again.retries {
            *outcome.retries.entry(id).or_default() += n;
        }
        for (id, n) in again.rollbacks {
            *outcome.rollbacks.entry(id).or_default() += n;
        }
    }

    for _ in 0..LINT_PRUNE_ROUNDS {
        let lint = verify::clippy(&crate_dir, &assemble(&graph)).map_err(|e| e.to_string())?;
        if lint.ok {
            break;
        }
        let removed = lintfix::prune(&mut graph, &lint.by_node);
        if removed.is_empty() {
            break;
        }
        eprintln!("  pruned {} unused nodes", removed.len());
        current = record_view(&mut store, &graph, &run_id, Some(&current))?;
    }

    let mut swept = 0;
    let swaps: Vec<_> = ledger::sweep_plan(&store, &current)
        .map_err(io)?
        .into_iter()
        .filter(|(node, _)| !graph.frozen.contains(node))
        .collect();
    if !swaps.is_empty() {
        let before = all_tests(&crate_dir, &graph);
        let mut candidate = graph.clone();
        for (node, version) in &swaps {
            let body = store.get_version(version).map_err(io)?.body;
            if let Some(n) = candidate.nodes.get_mut(node) {
                n.body = body;
            }
        }
        let after = all_tests(&crate_dir, &candidate);
        let better = after.compiled && (!before.compiled || failing(&after) <= failing(&before));
        eprintln!(
            "sweep: {} nodes back to their last good version: {} failing tests before, {} after{}",
            swaps.len(),
            failing(&before),
            failing(&after),
            if better { "" } else { " (kept the current code)" }
        );
        if better {
            graph = candidate;
            current = record_view(&mut store, &graph, &run_id, Some(&current))?;
            swept = swaps.len();
        }
    }

    let final_report = all_tests(&crate_dir, &graph);
    let lint_report = verify::clippy(&crate_dir, &assemble(&graph)).map_err(|e| e.to_string())?;
    store
        .put_results(Results {
            view: current.id.clone(),
            tests: cli::outcomes_of(&final_report),
            benches: Vec::new(),
            clippy_clean: Some(lint_report.ok),
            recorded_ms: now_ms(),
        })
        .map_err(io)?;
    let oracle_ok = match &args.oracle {
        Some(path) => Some(run_oracle(&crate_dir, &graph, path)?),
        None => None,
    };

    let ledger_all = ledger::ledger(&store).map_err(io)?;
    let failures: BTreeMap<NodeId, String> = outcome.failed.iter().cloned().collect();
    let reports = handoff::node_reports(&graph, &final_report, &failures, &store);
    let handoff_dir = args.handoff.clone().unwrap_or_else(|| {
        let mut name = crate_dir.as_os_str().to_owned();
        name.push("-handoff");
        PathBuf::from(name)
    });
    handoff::write_repo(
        &store,
        &current.id,
        Some(&crate_dir.join("Cargo.toml")),
        &handoff_dir,
        &format!(
            "{}{}{}",
            handoff::render(&graph, &reports),
            ledger::summary_markdown(&ledger_all),
            review_section(&review_notes)
        ),
    )?;

    let mut metrics = RunMetrics::default();
    for (id, n) in &outcome.retries {
        for _ in 0..*n {
            metrics.record_retry(id);
        }
    }
    metrics.input_tokens = usage.input.load(Ordering::Relaxed);
    metrics.output_tokens = usage.output.load(Ordering::Relaxed);
    metrics.wall_ms = started.elapsed().as_millis() as u64;

    outcome
        .failed
        .retain(|(id, _)| graph.nodes.get(id).is_some_and(|n| !n.is_filled()));
    let ok = outcome.failed.is_empty()
        && untested.is_empty()
        && final_report.compiled
        && failing(&final_report) == 0
        && lint_report.ok
        && oracle_ok.unwrap_or(true);
    let summary = serde_json::json!({
        "ok": ok,
        "view": current.id.0,
        "store": store_dir.display().to_string(),
        "handoff": handoff_dir.display().to_string(),
        "structural": reports.iter().filter(|r| !r.structure.is_empty()).map(|r| format!("{}: {}", r.node.0, r.structure.join("; "))).collect::<Vec<_>>(),
        "start_here": handoff::start_here(&graph, &reports).iter().map(|n| n.0.clone()).collect::<Vec<_>>(),
        "nodes": graph.nodes.len(),
        "tests": test_count,
        "failed_nodes": outcome.failed.iter().map(|(id, why)| format!("{}: {why}", id.0)).collect::<Vec<_>>(),
        "untested_nodes": untested.iter().map(|(id, _)| id.0.clone()).collect::<Vec<_>>(),
        "failing_tests": failing(&final_report),
        "skeleton_review": review_notes,
        "swept_to_last_good": swept,
        "failures_seen": ledger_all.len(),
        "failures_open": ledger_all.iter().filter(|f| f.is_open()).count(),
        "rollbacks": outcome.rollbacks.iter().map(|(id, n)| (id.0.clone(), *n)).collect::<BTreeMap<_, _>>(),
        "clippy_clean": lint_report.ok,
        "oracle_passed": oracle_ok,
        "metrics": serde_json::from_str::<serde_json::Value>(&metrics.to_json()).unwrap_or_default(),
    });
    println!("{summary:#}");
    Ok(ok)
}

fn review_section(notes: &[String]) -> String {
    if notes.is_empty() {
        return String::new();
    }
    let mut out = String::from("## Skeleton review\n\n");
    for note in notes {
        out.push_str(&format!("- {note}\n"));
    }
    out.push('\n');
    out
}

fn history_cmd(args: Vec<String>) -> Result<(), String> {
    let flags = Flags::parse(args, &["--node"], &[])?;
    let [store] = <[String; 1]>::try_from(flags.positional.clone())
        .map_err(|_| "expected <store>".to_string())?;
    let store = open_store(&store)?;
    let node = flags.values.get("--node").map(|n| NodeId(n.clone()));
    print!("{}", cli::history(&store, node.as_ref())?);
    Ok(())
}

fn failures_cmd(args: Vec<String>) -> Result<(), String> {
    let flags = Flags::parse(args, &["--node"], &["--open"])?;
    let [store] = <[String; 1]>::try_from(flags.positional.clone())
        .map_err(|_| "expected <store>".to_string())?;
    let node = flags.values.get("--node").map(|n| NodeId(n.clone()));
    print!(
        "{}",
        cli::failures(&open_store(&store)?, node.as_ref(), flags.has("--open"))?
    );
    Ok(())
}

fn good_cmd(args: Vec<String>) -> Result<(), String> {
    let [store, node] =
        <[String; 2]>::try_from(args).map_err(|_| "expected <store> <node>".to_string())?;
    print!("{}", cli::good(&open_store(&store)?, &NodeId(node))?);
    Ok(())
}

fn sweep_cmd(args: Vec<String>) -> Result<(), String> {
    let flags = Flags::parse(args, &[], &["--dry-run"])?;
    let [store, view] = <[String; 2]>::try_from(flags.positional.clone())
        .map_err(|_| "expected <store> <view>".to_string())?;
    let mut store = open_store(&store)?;
    let (text, _) = cli::sweep(&mut store, &view, !flags.has("--dry-run"), now_ms())?;
    print!("{text}");
    Ok(())
}

fn diff_cmd(args: Vec<String>) -> Result<(), String> {
    let [store, a, b] =
        <[String; 3]>::try_from(args).map_err(|_| "expected <store> <a> <b>".to_string())?;
    print!("{}", cli::diff_views(&open_store(&store)?, &a, &b)?);
    Ok(())
}

fn undo_cmd(args: Vec<String>) -> Result<(), String> {
    let [store, view] =
        <[String; 2]>::try_from(args).map_err(|_| "expected <store> <view>".to_string())?;
    println!("{}", cli::undo(&open_store(&store)?, &view)?.0);
    Ok(())
}

fn swap_cmd(args: Vec<String>) -> Result<(), String> {
    let [store, view, node, version] = <[String; 4]>::try_from(args)
        .map_err(|_| "expected <store> <view> <node> <version>".to_string())?;
    let mut store = open_store(&store)?;
    let id = cli::swap_cmd(&mut store, &view, &NodeId(node), &version, now_ms())?;
    println!("{}", id.0);
    Ok(())
}

fn export_cmd(args: Vec<String>) -> Result<(), String> {
    let flags = Flags::parse(args, &[], &["--commit"])?;
    let [store, view, dir] = <[String; 3]>::try_from(flags.positional.clone())
        .map_err(|_| "expected <store> <view> <dir>".to_string())?;
    let commit = flags.has("--commit");
    let id = cli::export_cmd(&open_store(&store)?, &view, Path::new(&dir), commit)?;
    println!("exported view {}", id.0);
    Ok(())
}

fn ab_cmd(args: Vec<String>) -> Result<(), String> {
    let flags = Flags::parse(
        args,
        &["--spec", "--crate", "--n", "--runs", "--noise", "--width"],
        &[],
    )?;
    let [store_dir, view_arg, node_arg] = <[String; 3]>::try_from(flags.positional.clone())
        .map_err(|_| "expected <store> <view> <node>".to_string())?;
    let spec = std::fs::read_to_string(flags.required("--spec")?).map_err(io)?;
    let crate_dir = flags.required("--crate")?;
    let n: usize = flags.number("--n", 4)?;
    let cfg = BenchConfig {
        runs: flags.number("--runs", 7)?,
        noise_pct: flags.number("--noise", 5.0)?,
    };
    let width = flags.number("--width", n)?;

    let mut store = open_store(&store_dir)?;
    let base_id = cli::resolve_view(&store, &view_arg)?;
    let mut base = store.get_view(&base_id).map_err(io)?;
    let node = cli::parse_node(&base, &node_arg)?;
    let registry = Arc::new(Registry::from_config(&Config::load()));
    let usage = Arc::new(Usage::default());
    let run_id = new_run_id();

    let mut graph = view::to_graph(&store, &base).map_err(io)?;
    graph.spec = spec.clone();
    if cli::benches_reaching(&graph, &node).is_empty() {
        eprintln!("no bench reaches {}; generating one", node.0);
        let mut gen = registry_generator(&registry, &usage);
        let mut check = |g: &Graph| -> Result<(), String> {
            let needed = cli::benches_reaching(g, &node);
            if needed.is_empty() {
                return Err("no bench reaches the node".into());
            }
            let results = bench::run_benches(&crate_dir, &assemble(g), &[], &cfg).map_err(io)?;
            let missing: Vec<String> = needed
                .iter()
                .filter(|id| !results.iter().any(|r| &r.bench == *id))
                .map(|id| id.0.clone())
                .collect();
            if missing.is_empty() {
                Ok(())
            } else {
                Err(format!("no BENCH output from: {}", missing.join(", ")))
            }
        };
        benchgen::generate(
            &mut graph,
            &spec,
            &node,
            &mut gen,
            &mut check,
            BENCH_RETRIES,
        )?;
        base = record_view(&mut store, &graph, &run_id, Some(&base))?;
        eprintln!("bench view {}", base.id.0);
    }

    let mut evaluate = |g: &Graph| -> Evaluation {
        let assembled = assemble(g);
        let tests = verify::run_tests(&crate_dir, &assembled, &[]).unwrap_or_default();
        let passed = tests.compiled && tests.outcomes.iter().all(|o| o.passed);
        let benches = if passed {
            bench::run_benches(&crate_dir, &assembled, &[], &cfg).unwrap_or_default()
        } else {
            Vec::new()
        };
        Evaluation { tests, benches }
    };
    let mut generators = generator_pool(&registry, &usage, width);
    let report = variants::tournament(
        &mut store,
        &base,
        &node,
        n,
        &spec,
        &mut generators,
        &mut evaluate,
        &cfg,
        &run_id,
    )
    .map_err(io)?;
    print!("{}", cli::format_report(&report));
    Ok(())
}

fn edit_cmd(args: Vec<String>) -> Result<(), String> {
    let flags = Flags::parse(args, &["--spec"], &[])?;
    let [store_dir, view_arg, crate_dir, ids @ ..] = flags.positional.as_slice() else {
        return Err("expected <store> <view> <crate_dir> <node ids...>".into());
    };
    if ids.is_empty() {
        return Err("name at least one node to edit".into());
    }
    let spec = std::fs::read_to_string(flags.required("--spec")?).map_err(io)?;
    let crate_dir = PathBuf::from(crate_dir);
    let mut store = open_store(store_dir)?;
    let base_id = cli::resolve_view(&store, view_arg)?;
    let base = store.get_view(&base_id).map_err(io)?;
    let mut graph = view::to_graph(&store, &base).map_err(io)?;
    graph.spec = spec;
    for id in ids {
        let id = cli::parse_node(&base, id)?;
        let node = graph.nodes.get_mut(&id).ok_or_else(|| id.0.clone())?;
        if !matches!(node.kind, NodeKind::Function | NodeKind::Method) {
            return Err(format!("{} has no body to rewrite", id.0));
        }
        node.body = None;
    }

    let registry = Arc::new(Registry::from_config(&Config::load()));
    let usage = Arc::new(Usage::default());
    let mut gen = registry_generator(&registry, &usage);
    let run_id = new_run_id();
    let mut run_tests = |g: &Graph, only: &[NodeId]| {
        verify::run_tests(&crate_dir, &assemble(g), only).unwrap_or_default()
    };
    let (outcome, edited) = fill_all_versioned(
        &mut graph,
        &mut gen,
        &mut run_tests,
        FILL_RETRIES,
        &mut store,
        &run_id,
        MODEL,
        Some(&base),
    )
    .map_err(io)?;
    for (id, why) in &outcome.failed {
        eprintln!("  failed {}: {why}", id.0);
    }
    let report = all_tests(&crate_dir, &graph);
    store
        .put_results(Results {
            view: edited.id.clone(),
            tests: cli::outcomes_of(&report),
            benches: Vec::new(),
            clippy_clean: None,
            recorded_ms: now_ms(),
        })
        .map_err(io)?;
    println!("{}", edited.id.0);
    if !outcome.failed.is_empty() || graph.nodes.values().any(|n| !n.is_filled()) {
        return Err("some nodes were not filled; crate directory left unchanged".into());
    }
    write_back(&crate_dir, &graph).map_err(io)
}

fn dispatch(args: Vec<String>) -> Result<bool, String> {
    let mut args = args;
    let command = args.first().cloned().unwrap_or_default();
    let known = [
        "history", "diff", "failures", "good", "sweep", "undo", "swap", "export", "ab", "edit", "run",
    ];
    if !known.contains(&command.as_str()) {
        return run(parse_args(args)?);
    }
    args.remove(0);
    match command.as_str() {
        "run" => return run(parse_args(args)?),
        "history" => history_cmd(args)?,
        "diff" => diff_cmd(args)?,
        "failures" => failures_cmd(args)?,
        "good" => good_cmd(args)?,
        "sweep" => sweep_cmd(args)?,
        "undo" => undo_cmd(args)?,
        "swap" => swap_cmd(args)?,
        "export" => export_cmd(args)?,
        "ab" => ab_cmd(args)?,
        _ => edit_cmd(args)?,
    }
    Ok(true)
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.is_empty() {
        eprintln!("{USAGE}");
        std::process::exit(2);
    }
    match dispatch(args) {
        Ok(true) => {}
        Ok(false) => std::process::exit(1),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
