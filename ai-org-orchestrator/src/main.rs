mod advice;
mod agents;
mod analysis;
mod debug;
mod estimation;
mod increments;
mod manager;
mod observer;
mod patch;
#[cfg(test)]
mod pipeline_structure;
mod plan_review;
mod provider_setup;
mod quality;
mod review_scope;
mod schema_check;
mod services;
mod speculate;
mod split_guard;
mod synthesis;
mod tools;
mod workspace;

use agents::*;
use manager::Manager;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use std::collections::HashMap;
use std::path::Path;
use tools::PipelineToolbox;
use workspace::{read_source_dir, Workspace};

struct Args {
    workspace: String,
    source: String,
    model: String,
    local_model: Option<String>,
    ollama_host: Option<String>,
    lmstudio_host: Option<String>,
    lmstudio_model: Option<String>,
    claude_cli_model: Option<String>,
    gemini_key: Option<String>,
    max_iter: usize,
}

fn parse_args() -> Args {
    let raw: Vec<String> = std::env::args().collect();
    let mut workspace = None;
    let mut source = None;
    let mut model = "claude-opus-4-7".to_string();
    let mut local_model: Option<String> = None;
    let mut ollama_host: Option<String> = None;
    let mut lmstudio_host: Option<String> = std::env::var("REWRITER_LMSTUDIO_HOST").ok();
    let mut lmstudio_model: Option<String> = std::env::var("REWRITER_LMSTUDIO_MODEL").ok();
    let mut claude_cli_model: Option<String> = provider_setup::claude_cli_model_from_env();
    let mut gemini_key: Option<String> = std::env::var("GEMINI_API_KEY").ok();
    let mut max_iter = 10usize;

    let mut i = 1;
    while i < raw.len() {
        match raw[i].as_str() {
            "--workspace" | "-w" => {
                i += 1;
                workspace = raw.get(i).cloned();
            }
            "--source" | "-s" => {
                i += 1;
                source = raw.get(i).cloned();
            }
            "--model" => {
                i += 1;
                model = raw.get(i).cloned().unwrap_or(model);
            }
            "--local-model" => {
                i += 1;
                local_model = raw.get(i).cloned();
            }
            "--ollama-host" => {
                i += 1;
                ollama_host = raw.get(i).cloned();
            }
            "--max-iter" => {
                i += 1;
                max_iter = raw.get(i).and_then(|s| s.parse().ok()).unwrap_or(10);
            }
            "--lmstudio-host" => {
                i += 1;
                lmstudio_host = raw.get(i).cloned();
            }
            "--lmstudio-model" => {
                i += 1;
                lmstudio_model = raw.get(i).cloned();
            }
            "--claude-cli" => {
                // --claude-cli alone defaults to Haiku; --claude-cli <model> overrides
                let next = raw.get(i + 1);
                if next.map(|s| !s.starts_with('-')).unwrap_or(false) {
                    i += 1;
                    claude_cli_model = raw.get(i).cloned();
                } else {
                    claude_cli_model = Some("claude-haiku-4-5-20251001".into());
                }
            }
            "--gemini-key" => {
                i += 1;
                gemini_key = raw.get(i).cloned();
            }
            _ => {}
        }
        i += 1;
    }

    Args {
        workspace: workspace.expect("--workspace <dir> required"),
        source: source.expect("--source <dir> required"),
        model,
        local_model,
        ollama_host,
        lmstudio_host,
        lmstudio_model,
        claude_cli_model,
        gemini_key,
        max_iter,
    }
}

/// Fewest channels at which later stages are started early: one for the contract, one kept
/// back for the main path, and at least one to speculate on.
const SPECULATE_MIN_CHANNELS: usize = 3;
const SPECULATE_RESERVE: usize = 1;
const SPECULATIVE_MARKER: &str = "speculative.json";
/// Everything speculation may write under its final name, so it can be removed again.
const SPECULATIVE_ARTIFACTS: [&str; 4] = [
    "inductive_analysis.md",
    "schema.rs",
    "refined_test_matrix.json",
    "acceptance.feature",
];

/// Record that early results may exist and are not yet confirmed. If the process dies before
/// they are confirmed or discarded, the next start removes them rather than trusting them.
fn mark_speculative(ws: &Workspace) {
    let _ = ws.emit_artifact(SPECULATIVE_MARKER, "early results written before the contract was final");
}

fn clear_speculative_marker(ws: &Workspace) {
    ws.remove_artifact(SPECULATIVE_MARKER);
}

/// Remove early results that were never confirmed.
fn discard_speculative(ws: &Workspace) {
    if ws.get_artifact(SPECULATIVE_MARKER).is_ok() {
        for name in SPECULATIVE_ARTIFACTS {
            ws.remove_artifact(name);
        }
        clear_speculative_marker(ws);
    }
}

fn author_contract(
    mgr: &Manager,
    mission: &str,
    max_iter: usize,
    toolbox: &(dyn tools::Toolbox + Sync),
) -> Result<String, Box<dyn std::error::Error>> {
    let contract_task = format!(
        "# Mission\n{mission}\n\n\
         Produce the ObjectiveContract. V1 source has not been pasted in — read whatever \
         files you actually need with your tools (list_files/read_file). Record \
         deviations you observe — do not implement them."
    );
    mgr.run_with_review(
        "MissionArchitect",
        MISSION_ARCHITECT,
        "ContractReviewer",
        CONTRACT_REVIEWER,
        &contract_task,
        "APPROVED",
        max_iter,
        toolbox,
    )
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("analyze") {
        std::process::exit(analysis::run_cli(&std::env::args().skip(2).collect::<Vec<_>>()));
    }
    let args = parse_args();
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY").ok();
    let groq_key      = std::env::var("GROQ_API_KEY").ok();
    let openai_key    = std::env::var("OPENAI_API_KEY").ok();
    let cfg = inference_providers::config::Config::load();

    let observer = observer::Observer::from_env(Path::new(&args.workspace));
    observer.install_panic_hook();

    if anthropic_key.is_none() && groq_key.is_none() && openai_key.is_none()
        && args.local_model.is_none() && args.gemini_key.is_none()
        && cfg.providers.is_empty()
    {
        let message = "error: set at least one of ANTHROPIC_API_KEY, GROQ_API_KEY, OPENAI_API_KEY, \
             GEMINI_API_KEY, or configure [[providers]] in ~/.config/rewriter/config.toml";
        eprintln!("{message}");
        observer.fail("orchestrator", message);
        std::process::exit(1);
    }

    let ws = Workspace::new(&args.workspace)
        .expect("cannot open workspace")
        .with_observer(observer);

    let registry = provider_setup::build_registry(
        &cfg,
        provider_setup::ProviderOptions {
            anthropic_key,
            groq_key,
            openai_key,
            local_model: args.local_model.clone(),
            ollama_host: args.ollama_host.clone(),
            lmstudio_host: args.lmstudio_host.clone(),
            lmstudio_model: args.lmstudio_model.clone(),
            claude_cli_model: args.claude_cli_model.clone(),
            gemini_key: args.gemini_key.clone(),
        },
    );

    // Probe each provider for context limit and quality before any real work.
    registry.run_qq();

    let mgr = Manager::new(&ws, &registry)
        .with_estimation_history(estimation::history_path())
        .with_answers_at_high_effort();

    // The mission is itself a pipeline artifact -- read the same way as objective_contract.md,
    // schema.rs, and everything else, and just as uniformly readable via the `read_artifact`
    // tool. There is no sensible default for "what should this system even do": a missing or
    // blank mission means MissionArchitect infers the whole ObjectiveContract from raw source
    // with no stated intent, which is worse than refusing to start.
    let mission = match ws.get_artifact("mission.md") {
        Ok(m) if !m.trim().is_empty() => m,
        _ => {
            eprintln!(
                "error: no mission set. Write a short statement of what V2 should actually do \
                 (not just \"port this file\") to {}/artifacts/mission.md before submitting -- \
                 the mission is itself a required artifact, the same as every other pipeline \
                 artifact, not optional scaffolding.",
                args.workspace
            );
            ws.observer().inspect(|o| o.fail("orchestrator", "no mission set"));
            std::process::exit(1);
        }
    };
    eprintln!("Mission: {} bytes", mission.len());

    if let Err(e) = services::start_declared(&mission) {
        eprintln!("error: could not start the services the mission declares: {e}");
        services::stop_all();
        ws.observer().inspect(|o| o.fail("orchestrator", &format!("service start failed: {e}")));
        std::process::exit(1);
    }

    if !tools::source_dir_has_readable_files(Path::new(&args.source)) {
        eprintln!(
            "error: no readable source files found under {} (empty dir, or every file is binary/non-UTF-8)",
            args.source
        );
        ws.observer().inspect(|o| o.fail("orchestrator", "no readable source files found"));
        std::process::exit(1);
    }

    // Emit V2 Cargo.toml now so the quality gate can run cargo during synthesis.
    let source_cargo = std::fs::read_to_string(Path::new(&args.source).join("Cargo.toml"))
        .unwrap_or_default();
    let v2_cargo = if source_cargo.trim().is_empty() {
        // Non-Rust source (JS, Python, ...) has no manifest to inherit from.
        // resolve_cargo_toml assumes a real source Cargo.toml exists to rewrite;
        // fed an empty one, it produces nothing but a bare `[workspace]` opt-out
        // line -- no [package] section, so `cargo build` correctly refuses it
        // ("manifest is virtual, and the workspace has no members"). Seed a real
        // standalone package instead; TargetImplementer can still patch in
        // dependencies it needs via the [dependencies] section already present.
        "[package]\nname = \"v2\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n"
            .to_string()
    } else {
        let ws_cargo = find_workspace_cargo(Path::new(&args.source)).unwrap_or_default();
        resolve_cargo_toml(&source_cargo, &ws_cargo, "v2")
    };
    ws.emit_artifact("v2/Cargo.toml", &v2_cargo).unwrap();

    // Every early stage below reads V1 source and prior artifacts on demand instead of
    // having them pasted into its prompt (see `tools::PipelineToolbox`) -- a fresh toolbox
    // per stage, since `fan_out` sub-tasks are answered by "the same role" as whichever
    // stage spawned them.
    let toolbox_for = |agent: &str, system: &str| {
        PipelineToolbox::new(&args.source, &ws, &mgr, agent, system, manager::AGENTIC_MAX_TURNS)
    };

    // The stages after the contract. Each takes the manager to run on, so speculative work can
    // use one that can be cancelled; `announce` sets the stage label and `emit` writes the
    // checkpoint. They return errors instead of panicking so that a cancelled run is quiet.
    let acceptance_stage = |m: &Manager, announce: bool, emit: bool| -> Result<String, String> {
        if let Ok(cached) = ws.get_artifact("acceptance.feature") {
            eprintln!("==> Acceptance criteria (resuming from checkpoint)");
            return Ok(cached);
        }
        eprintln!("==> Acceptance criteria");
        if announce {
            ws.observer().inspect(|o| o.stage("Acceptance criteria"));
        }
        let ac_task = "Read the ObjectiveContract with read_artifact(\"objective_contract.md\"), \
             then write Gherkin acceptance criteria covering its key behaviors and edge cases. \
             Read V1 source with list_files/read_file only if you need to confirm a concrete \
             detail the contract doesn't spell out.";
        let toolbox = PipelineToolbox::new(
            &args.source, &ws, m, "AcceptanceCriteria", ACCEPTANCE_CRITERIA, manager::AGENTIC_MAX_TURNS,
        );
        let g = m
            .run_agentic("AcceptanceCriteria", ACCEPTANCE_CRITERIA, ac_task, &toolbox, manager::AGENTIC_MAX_TURNS)
            .map_err(|e| e.to_string())?;
        if emit {
            ws.emit_artifact("acceptance.feature", &g).map_err(|e| e.to_string())?;
        }
        Ok(g)
    };

    let test_matrix_stage = |m: &Manager, announce: bool, emit: bool| -> Result<String, String> {
        if let Ok(cached) = ws.get_artifact("refined_test_matrix.json") {
            eprintln!("==> Test matrix refinement (resuming from checkpoint)");
            return Ok(cached);
        }
        eprintln!("==> Test matrix refinement");
        if announce {
            ws.observer().inspect(|o| o.stage("Test matrix refinement"));
        }
        let raw_matrix = ws.read_test_matrix().unwrap_or_default();
        let te_task = format!(
            "# Raw Test Matrix\n{raw_matrix}\n\n\
             Read the ObjectiveContract with read_artifact(\"objective_contract.md\"), then \
             read whatever V1 source files you need with list_files/read_file. For each test \
             case, determine concrete inputs and expected V1 outputs."
        );
        let toolbox = PipelineToolbox::new(
            &args.source, &ws, m, "TestEngineer", TEST_ENGINEER, manager::AGENTIC_MAX_TURNS,
        );
        let r = m
            .run_agentic("TestEngineer", TEST_ENGINEER, &te_task, &toolbox, manager::AGENTIC_MAX_TURNS)
            .map_err(|e| e.to_string())?;
        if emit {
            ws.emit_artifact("refined_test_matrix.json", &r).map_err(|e| e.to_string())?;
        }
        Ok(r)
    };

    let inductive_stage = |m: &Manager, announce: bool, emit: bool| -> Result<String, String> {
        if let Ok(cached) = ws.get_artifact("inductive_analysis.md") {
            eprintln!("==> Inductive analysis (resuming from checkpoint)");
            return Ok(cached);
        }
        eprintln!("==> Inductive analysis");
        if announce {
            ws.observer().inspect(|o| o.stage("Inductive analysis"));
        }
        let ir_task = "Read the ObjectiveContract with read_artifact(\"objective_contract.md\"), \
             then analyze V1 as a system using list_files/read_file. Surface implicit \
             invariants, systemic patterns, architectural analogies, and risk zones. For a \
             large source tree, use fan_out to analyze independent subsystems concurrently, \
             then weave the results together yourself.";
        let toolbox = PipelineToolbox::new(
            &args.source, &ws, m, "InductiveReasoner", INDUCTIVE_REASONER, manager::AGENTIC_MAX_TURNS,
        );
        let i = m
            .run_agentic("InductiveReasoner", INDUCTIVE_REASONER, ir_task, &toolbox, manager::AGENTIC_MAX_TURNS)
            .map_err(|e| e.to_string())?;
        if emit {
            ws.emit_artifact("inductive_analysis.md", &i).map_err(|e| e.to_string())?;
        }
        Ok(i)
    };

    let schema_stage = |m: &Manager, announce: bool, emit: bool| -> Result<String, String> {
        if let Ok(cached) = ws.get_artifact("schema.rs") {
            eprintln!("==> Schema (resuming from checkpoint)");
            return Ok(cached);
        }
        eprintln!("==> Schema");
        if announce {
            ws.observer().inspect(|o| o.stage("Schema"));
        }
        let schema_task = "Read the ObjectiveContract (read_artifact(\"objective_contract.md\")) \
             and the Inductive Analysis (read_artifact(\"inductive_analysis.md\")), then read \
             whatever V1 source you need with list_files/read_file. Design the Rust type \
             system and public API stubs for V2. Preserve the implicit invariants identified \
             in the Inductive Analysis.";
        let toolbox = PipelineToolbox::new(
            &args.source, &ws, m, "SchemaArchitect", SCHEMA_ARCHITECT, manager::AGENTIC_MAX_TURNS,
        );
        let s = m
            .run_agentic("SchemaArchitect", SCHEMA_ARCHITECT, schema_task, &toolbox, manager::AGENTIC_MAX_TURNS)
            .map_err(|e| e.to_string())?;
        let s = Manager::strip_fences(&s).to_string();
        if emit {
            ws.emit_artifact("schema.rs", &s).map_err(|e| e.to_string())?;
        }
        Ok(s)
    };

    // ── 1. ObjectiveContract ──────────────────────────────────────────────────
    // With channels to spare, the stages that follow start from the contract's first settled
    // draft while the architect and reviewer carry on. If the approved contract is the draft,
    // what they produced stands as the checkpoints; if it changed, it is thrown away and any
    // of it still running is cancelled. The contract is the only thing they are built on.
    let contract = if let Ok(cached) = ws.get_artifact("objective_contract.md") {
        eprintln!("==> ObjectiveContract (resuming from checkpoint)");
        cached
    } else {
        eprintln!("==> ObjectiveContract");
        ws.observer().inspect(|o| o.stage("ObjectiveContract"));
        discard_speculative(&ws);
        let toolbox = toolbox_for("MissionArchitect", MISSION_ARCHITECT);
        if mgr.registry.capable_heavy_slots() >= SPECULATE_MIN_CHANNELS {
            let finished = AtomicBool::new(false);
            let cancel = Arc::new(AtomicBool::new(false));
            let draft = Mutex::new(None::<u64>);
            let (c, lane) = std::thread::scope(|scope| {
                let lane = scope.spawn(|| {
                    let text = speculate::wait_for_draft(
                        &|| ws.get_artifact("objective_contract.md").ok(),
                        &finished,
                        Duration::from_secs(15),
                        Duration::from_secs(2),
                    )?;
                    *draft.lock().unwrap() = Some(speculate::fingerprint(&text));
                    eprintln!(
                        "    contract draft settled: starting later stages on {} spare channels",
                        mgr.registry.free_channels()
                    );
                    mark_speculative(&ws);
                    let spec = Manager::new(&ws, mgr.registry).with_cancel(cancel.clone());
                    let tasks = vec![
                        speculate::Task {
                            name: "inductive",
                            priority: speculate::priority(3, 0.7),
                            requires: None,
                            run: Box::new(|| inductive_stage(&spec, false, true)),
                        },
                        speculate::Task {
                            name: "schema",
                            priority: speculate::priority(2, 0.8),
                            requires: Some("inductive"),
                            run: Box::new(|| schema_stage(&spec, false, true)),
                        },
                        speculate::Task {
                            name: "test matrix",
                            priority: speculate::priority(1, 0.9),
                            requires: None,
                            run: Box::new(|| test_matrix_stage(&spec, false, true)),
                        },
                        speculate::Task {
                            name: "acceptance",
                            priority: speculate::priority(1, 0.3),
                            requires: None,
                            run: Box::new(|| acceptance_stage(&spec, false, true)),
                        },
                    ];
                    Some(speculate::run(
                        tasks,
                        &|| mgr.registry.free_channels(),
                        SPECULATE_RESERVE,
                        &cancel,
                        Duration::from_millis(250),
                    ))
                });
                let c = author_contract(&mgr, &mission, args.max_iter, &toolbox)
                    .expect("contract phase failed");
                finished.store(true, Ordering::SeqCst);
                ws.emit_artifact("objective_contract.md", &c).unwrap();
                if *draft.lock().unwrap() != Some(speculate::fingerprint(&c)) {
                    cancel.store(true, Ordering::SeqCst);
                }
                let lane = lane.join().unwrap_or_else(|e| std::panic::resume_unwind(e));
                (c, lane)
            });
            match lane {
                None => eprintln!("    contract finished before a draft settled; nothing was started early"),
                Some(done) if *draft.lock().unwrap() == Some(speculate::fingerprint(&c)) => {
                    let kept: Vec<&str> =
                        done.iter().filter(|d| d.result.is_ok()).map(|d| d.name).collect();
                    eprintln!("    the contract is its draft: kept {} early result(s): {}", kept.len(), kept.join(", "));
                    clear_speculative_marker(&ws);
                }
                Some(_) => {
                    eprintln!("    the contract changed after its draft: discarding the early results");
                    discard_speculative(&ws);
                }
            }
            c
        } else {
            let c = author_contract(&mgr, &mission, args.max_iter, &toolbox)
                .expect("contract phase failed");
            ws.emit_artifact("objective_contract.md", &c).unwrap();
            c
        }
    };
    eprintln!("    contract done");

    // ── 2 to 5. Acceptance criteria, test matrix, inductive analysis, schema ──
    // Whatever speculation kept is already a checkpoint and is skipped here. What is left runs
    // as before: acceptance criteria and the test matrix beside the inductive analysis and
    // the schema when the providers can serve more than one request at once.
    let concurrent = mgr.registry.capable_heavy_slots() >= 2;
    let (gherkin, inductive, schema) = if concurrent {
        eprintln!(
            "    {} parallel slots: acceptance criteria and test matrix run beside the inductive analysis and schema",
            mgr.registry.capable_heavy_slots()
        );
        ws.observer().inspect(|o| o.stage("Inductive analysis (acceptance criteria and test matrix alongside)"));
        std::thread::scope(|scope| {
            let acceptance = scope.spawn(|| acceptance_stage(&mgr, false, true));
            let test_matrix = scope.spawn(|| test_matrix_stage(&mgr, false, true));
            let inductive = inductive_stage(&mgr, false, true).expect("inductive reasoner failed");
            eprintln!("    inductive analysis done");
            let schema = schema_stage(&mgr, true, true).expect("schema phase failed");
            eprintln!("    schema done");
            let gherkin = acceptance
                .join()
                .unwrap_or_else(|e| std::panic::resume_unwind(e))
                .expect("acceptance criteria phase failed");
            test_matrix
                .join()
                .unwrap_or_else(|e| std::panic::resume_unwind(e))
                .expect("test engineer failed");
            eprintln!("    acceptance criteria and test matrix done");
            (gherkin, inductive, schema)
        })
    } else {
        let gherkin = acceptance_stage(&mgr, true, true).expect("acceptance criteria phase failed");
        eprintln!("    acceptance criteria done");
        test_matrix_stage(&mgr, true, true).expect("test engineer failed");
        eprintln!("    test matrix refined");
        let inductive = inductive_stage(&mgr, true, true).expect("inductive reasoner failed");
        eprintln!("    inductive analysis done");
        let schema = schema_stage(&mgr, true, true).expect("schema phase failed");
        eprintln!("    schema done");
        (gherkin, inductive, schema)
    };

    // ── 5b. Schema must compile before anything is built on it ───────────────
    let schema = {
        let mut schema = schema;
        let mut repaired = false;
        for attempt in 0..=schema_check::SCHEMA_REPAIR_ATTEMPTS {
            if let Some(manifest) = mgr.schema_manifest(&schema) {
                ws.emit_artifact("v2/Cargo.toml", &manifest).unwrap();
            }
            let errors = mgr.schema_errors(&schema);
            if errors.is_empty() {
                break;
            }
            if attempt == schema_check::SCHEMA_REPAIR_ATTEMPTS {
                eprintln!("error: the schema still does not compile after {attempt} repair attempt(s):\n{errors}");
                ws.observer().inspect(|o| o.fail("SchemaArchitect", "schema does not compile"));
                services::stop_all();
                std::process::exit(1);
            }
            eprintln!("==> Schema does not compile — repair attempt {}", attempt + 1);
            let manifest = std::fs::read_to_string(ws.v2_dir(None).join("Cargo.toml")).unwrap_or_default();
            let repair_task = format!(
                "This schema failed `cargo check`. Fix it and output the complete corrected \
                 schema. Declare the crates it needs in a `// === Cargo.toml ===` section \
                 (with only features that exist in the version you pick) followed by \
                 `// === src/lib.rs ===` and the code.\n\n# Current manifest\n```\n{manifest}\n```\n\n\
                 # Schema\n```rust\n{schema}\n```\n\n# cargo check output\n```\n{errors}\n```"
            );
            let toolbox = toolbox_for("SchemaArchitect", SCHEMA_ARCHITECT);
            let fixed = mgr
                .run_agentic("SchemaArchitect", SCHEMA_ARCHITECT, &repair_task, &toolbox, manager::AGENTIC_MAX_TURNS)
                .expect("schema repair failed");
            schema = Manager::strip_fences(&fixed).to_string();
            ws.emit_artifact("schema.rs", &schema).unwrap();
            repaired = true;
        }
        if repaired {
            ws.reset_synthesis();
        }
        schema
    };

    // ── 5c. A schema this size means the mission is too big for one run ──────
    let schema_lines = increments::schema_rust_lines(&schema);
    if schema_lines > increments::SCHEMA_LINE_BUDGET && increments::split_requested(&mission) {
        eprintln!(
            "==> The schema is {schema_lines} lines against a budget of {} — this mission is too big for one run",
            increments::SCHEMA_LINE_BUDGET
        );
        if ws.root.join("chain.json").exists() {
            eprintln!(
                "error: this run is already one increment of a larger mission and its own schema is still \
                 {schema_lines} lines. Split the mission into smaller increments by hand."
            );
            ws.observer().inspect(|o| o.fail("orchestrator", "an increment is still too big for one run"));
            services::stop_all();
            std::process::exit(1);
        }
        ws.observer().inspect(|o| o.stage("Increment planning"));
        let planned = match mgr.plan_increments(schema_lines) {
            Ok(planned) => planned,
            Err(e) => {
                eprintln!("error: {e}");
                ws.observer().inspect(|o| o.fail("IncrementPlanner", &e.to_string()));
                services::stop_all();
                std::process::exit(1);
            }
        };
        ws.emit_artifact("increments.json", &increments::increments_json(schema_lines, &planned)).unwrap();
        eprintln!(
            "    planned {} increments: {} — the queue will submit them as jobs",
            planned.len(),
            planned.iter().map(|i| i.slug.as_str()).collect::<Vec<_>>().join(", ")
        );
        ws.observer().inspect(|o| o.end_stage());
        ws.log_retrospective(&format!(
            "Schema came to {schema_lines} lines against a budget of {}; the mission was split into {} increments (artifacts/increments.json).",
            increments::SCHEMA_LINE_BUDGET,
            planned.len()
        ))
        .unwrap();
        services::stop_all();
        return;
    }

    // ── 6. V2 synthesis (hourglass: fan-out → merge → review) ────────────────
    let v2 = if let Ok(cached) = ws.get_artifact("v2.rs") {
        eprintln!("==> V2 synthesis (resuming from checkpoint)");
        cached
    } else {
        eprintln!("==> V2 synthesis");
        ws.observer().inspect(|o| o.stage("V2 synthesis"));
        // Only the legacy byte-size chunk fan-out path (large source, multiple heavy
        // providers) still needs the literal concatenated text, for splitting -- the
        // milestone path above reads source on demand via `toolbox` instead.
        let v1_source = read_source_dir(Path::new(&args.source)).expect("cannot read source dir");
        let test_matrix = ws.read_test_matrix().unwrap_or_default();
        let toolbox = toolbox_for("MilestonePlanner", MILESTONE_PLANNER);
        let v = mgr
            .synthesize(
                TARGET_IMPLEMENTER,
                &contract,
                &schema,
                &v1_source,
                &test_matrix,
                &inductive,
                &gherkin,
                args.max_iter,
                &toolbox,
            )
            .expect("synthesis failed");
        ws.emit_artifact("v2.rs", &v).unwrap();
        v
    };
    eprintln!("    v2 done");
    services::stop_all();

    ws.observer().inspect(|o| o.end_stage());

    // ── Emit final V2 source (Cargo.toml already in place from pre-synthesis setup) ──
    eprintln!("==> Emitting V2 crate");
    emit_v2_source(&ws, &v2);
    eprintln!("    crate emitted → artifacts/v2/");

    // ── Done ──────────────────────────────────────────────────────────────────
    ws.log_retrospective(&format!(
        "Synthesis complete. Model: {}. Tokens: {}in/{}out. \
         Artifacts: objective_contract.md, schema.rs, refined_test_matrix.json, v2/. \
         Deviations recorded in deviations.md (deferred to next phase).",
        args.model, mgr.total_input_tokens(), mgr.total_output_tokens(),
    ))
    .unwrap();

    let v2_dir = format!("{}/artifacts/v2", args.workspace);
    eprintln!(
        "Done. Total tokens: {}in / {}out",
        mgr.total_input_tokens(), mgr.total_output_tokens()
    );
    eprintln!("Artifacts → {}/artifacts/", args.workspace);
    eprintln!("Build V2:  cargo build --manifest-path {v2_dir}/Cargo.toml");
    if ws.deviations_path().exists() {
        eprintln!("Deviations → {}/deviations.md", args.workspace);
    }
}

/// Write V2 source to `artifacts/v2/src/`. Handles multi-file output
/// (`// === src/filename.rs ===` markers) and strips code fences.
fn emit_v2_source(ws: &workspace::Workspace, raw: &str) {
    let code = Manager::strip_fences(raw);
    let sections = Manager::parse_file_sections(code);
    for (rel_path, content) in &sections {
        ws.emit_artifact(&format!("v2/{rel_path}"), content)
            .unwrap_or_else(|e| eprintln!("warn: could not write v2/{rel_path}: {e}"));
    }
}

/// Walk up from `source` to find the nearest Cargo.toml that contains `[workspace]`.
fn find_workspace_cargo(source: &Path) -> Option<String> {
    let mut dir = source.parent()?;
    loop {
        let candidate = dir.join("Cargo.toml");
        if candidate.exists() {
            let content = std::fs::read_to_string(&candidate).ok()?;
            if content.contains("[workspace]") {
                return Some(content);
            }
        }
        dir = dir.parent()?;
    }
}

/// Parse `[workspace.dependencies]` from a workspace Cargo.toml into name → spec.
fn parse_workspace_deps(workspace_toml: &str) -> HashMap<String, String> {
    let mut deps = HashMap::new();
    let mut in_section = false;

    for line in workspace_toml.lines() {
        if line.trim() == "[workspace.dependencies]" {
            in_section = true;
            continue;
        }
        if line.starts_with('[') {
            in_section = false;
        }
        if in_section && !line.trim().is_empty() && !line.starts_with('#') {
            if let Some(eq) = line.find('=') {
                let name = line[..eq].trim().to_string();
                let spec = line[eq + 1..].trim().to_string();
                deps.insert(name, spec);
            }
        }
    }
    deps
}

/// Produce a standalone Cargo.toml by resolving `{ workspace = true }` deps
/// and renaming the package to `new_name`.
fn resolve_cargo_toml(source_toml: &str, workspace_toml: &str, new_name: &str) -> String {
    let ws_deps = parse_workspace_deps(workspace_toml);
    let mut out: Vec<String> = Vec::new();
    let mut in_dep_section = false;
    let mut name_replaced = false;

    for line in source_toml.lines() {
        // Replace package name (only the first occurrence, before any [dependencies])
        if !name_replaced && !in_dep_section && line.trim_start().starts_with("name =") {
            out.push(format!("name = \"{new_name}\""));
            name_replaced = true;
            continue;
        }
        // Track section
        if line.starts_with('[') {
            in_dep_section = line == "[dependencies]"
                || line == "[dev-dependencies]"
                || line.starts_with("[dependencies.")
                || line.starts_with("[dev-dependencies.");
        }
        // Resolve workspace refs
        if in_dep_section && line.contains("workspace = true") {
            let dep_name = line.split('=').next().unwrap_or("").trim().to_string();
            if let Some(spec) = ws_deps.get(&dep_name) {
                out.push(format!("{dep_name} = {spec}"));
                continue;
            }
        }
        out.push(line.to_string());
    }

    let mut result = out.join("\n");
    // Opt out of any parent workspace so the crate is self-contained.
    if !result.contains("[workspace]") {
        result.push_str("\n\n[workspace]\n");
    }
    result
}
