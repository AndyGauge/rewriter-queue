mod agents;
mod manager;
mod patch;
mod quality;
mod synthesis;
mod tools;
mod workspace;

use agents::*;
use inference_providers::Registry;
use manager::Manager;
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
    let mut claude_cli_model: Option<String> = std::env::var("REWRITER_CLAUDE_CLI_MODEL")
        .ok()
        .or_else(|| std::env::var("REWRITER_USE_CLAUDE_CLI")
            .ok()
            .filter(|v| v == "1")
            .map(|_| "claude-haiku-4-5-20251001".into()));
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

fn main() {
    let args = parse_args();
    let anthropic_key = std::env::var("ANTHROPIC_API_KEY").ok();
    let groq_key      = std::env::var("GROQ_API_KEY").ok();
    let openai_key    = std::env::var("OPENAI_API_KEY").ok();
    let cfg = inference_providers::config::Config::load();

    if anthropic_key.is_none() && groq_key.is_none() && openai_key.is_none()
        && args.local_model.is_none() && args.gemini_key.is_none()
        && cfg.providers.is_empty()
    {
        eprintln!(
            "error: set at least one of ANTHROPIC_API_KEY, GROQ_API_KEY, OPENAI_API_KEY, GEMINI_API_KEY, \
             or configure [[providers]] in ~/.config/rewriter/config.toml"
        );
        std::process::exit(1);
    }

    let ws = Workspace::new(&args.workspace).expect("cannot open workspace");

    let mut registry = if cfg.providers.is_empty() {
        let ollama_models = args.local_model.as_ref()
            .map(|m| vec![m.clone()])
            .unwrap_or_default();
        Registry::from_env(anthropic_key, args.ollama_host.clone(), ollama_models)
    } else {
        Registry::from_config(&cfg)
    };

    if let Some(key) = groq_key {
        registry.add_openai_compat(
            "groq".into(),
            "https://api.groq.com/openai/v1".into(),
            key,
            vec!["llama-3.3-70b-versatile".into(), "llama-3.1-8b-instant".into()],
        );
    }
    if let Some(key) = openai_key {
        registry.add_openai_compat(
            "openai".into(),
            "https://api.openai.com/v1".into(),
            key,
            vec!["gpt-4o".into(), "gpt-4o-mini".into()],
        );
    }
    if let Some(host) = args.lmstudio_host {
        let model = args.lmstudio_model
            .unwrap_or_else(|| "qwen2.5-coder:32b".into());
        eprintln!("LM Studio: {host} model={model}");
        registry.add_openai_compat(
            "lmstudio".into(),
            format!("{}/v1", host.trim_end_matches('/')),
            String::new(), // no API key needed
            vec![model],
        );
    }
    if let Some(model) = args.claude_cli_model {
        registry.add_claude_cli(model);
    }
    if let Some(key) = args.gemini_key {
        registry.add_gemini(key, vec!["gemini-2.0-flash".into()]);
    }

    // Probe each provider for context limit and quality before any real work.
    registry.run_qq();

    let mgr = Manager::new(&ws, &registry);

    let mission = ws.get_mission();
    eprintln!("Mission: {} bytes", mission.len());

    if !tools::source_dir_has_readable_files(Path::new(&args.source)) {
        eprintln!(
            "error: no readable source files found under {} (empty dir, or every file is binary/non-UTF-8)",
            args.source
        );
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

    // ── 1. ObjectiveContract ──────────────────────────────────────────────────
    let contract = if let Ok(cached) = ws.get_artifact("objective_contract.md") {
        eprintln!("==> ObjectiveContract (resuming from checkpoint)");
        cached
    } else {
        eprintln!("==> ObjectiveContract");
        let contract_task = format!(
            "# Mission\n{mission}\n\n\
             Produce the ObjectiveContract. V1 source has not been pasted in — read whatever \
             files you actually need with your tools (list_files/read_file). Record \
             deviations you observe — do not implement them."
        );
        let toolbox = toolbox_for("MissionArchitect", MISSION_ARCHITECT);
        let c = mgr
            .run_with_review(
                "MissionArchitect",
                MISSION_ARCHITECT,
                "ContractReviewer",
                CONTRACT_REVIEWER,
                &contract_task,
                "APPROVED",
                args.max_iter,
                &toolbox,
            )
            .expect("contract phase failed");
        ws.emit_artifact("objective_contract.md", &c).unwrap();
        c
    };
    eprintln!("    contract done");

    // ── 2. Test matrix refinement ─────────────────────────────────────────────
    let refined = if let Ok(cached) = ws.get_artifact("refined_test_matrix.json") {
        eprintln!("==> Test matrix refinement (resuming from checkpoint)");
        cached
    } else {
        eprintln!("==> Test matrix refinement");
        let raw_matrix = ws.read_test_matrix().unwrap_or_default();
        let te_task = format!(
            "# Raw Test Matrix\n{raw_matrix}\n\n\
             Read the ObjectiveContract with read_artifact(\"objective_contract.md\"), then \
             read whatever V1 source files you need with list_files/read_file. For each test \
             case, determine concrete inputs and expected V1 outputs."
        );
        let toolbox = toolbox_for("TestEngineer", TEST_ENGINEER);
        let r = mgr
            .run_agentic("TestEngineer", TEST_ENGINEER, &te_task, &toolbox, manager::AGENTIC_MAX_TURNS)
            .expect("test engineer failed");
        ws.emit_artifact("refined_test_matrix.json", &r).unwrap();
        r
    };
    eprintln!("    test matrix refined");
    let _ = refined; // consumed via read_test_matrix below

    // ── 3. Inductive analysis ─────────────────────────────────────────────────
    let inductive = if let Ok(cached) = ws.get_artifact("inductive_analysis.md") {
        eprintln!("==> Inductive analysis (resuming from checkpoint)");
        cached
    } else {
        eprintln!("==> Inductive analysis");
        let ir_task = "Read the ObjectiveContract with read_artifact(\"objective_contract.md\"), \
             then analyze V1 as a system using list_files/read_file. Surface implicit \
             invariants, systemic patterns, architectural analogies, and risk zones. For a \
             large source tree, use fan_out to analyze independent subsystems concurrently, \
             then weave the results together yourself.";
        let toolbox = toolbox_for("InductiveReasoner", INDUCTIVE_REASONER);
        let i = mgr
            .run_agentic("InductiveReasoner", INDUCTIVE_REASONER, ir_task, &toolbox, manager::AGENTIC_MAX_TURNS)
            .expect("inductive reasoner failed");
        ws.emit_artifact("inductive_analysis.md", &i).unwrap();
        i
    };
    eprintln!("    inductive analysis done");

    // ── 4. Schema ─────────────────────────────────────────────────────────────
    let schema = if let Ok(cached) = ws.get_artifact("schema.rs") {
        eprintln!("==> Schema (resuming from checkpoint)");
        cached
    } else {
        eprintln!("==> Schema");
        let schema_task = "Read the ObjectiveContract (read_artifact(\"objective_contract.md\")) \
             and the Inductive Analysis (read_artifact(\"inductive_analysis.md\")), then read \
             whatever V1 source you need with list_files/read_file. Design the Rust type \
             system and public API stubs for V2. Preserve the implicit invariants identified \
             in the Inductive Analysis.";
        let toolbox = toolbox_for("SchemaArchitect", SCHEMA_ARCHITECT);
        let s = mgr
            .run_agentic("SchemaArchitect", SCHEMA_ARCHITECT, schema_task, &toolbox, manager::AGENTIC_MAX_TURNS)
            .expect("schema phase failed");
        ws.emit_artifact("schema.rs", &s).unwrap();
        s
    };
    eprintln!("    schema done");

    // ── 5. V2 synthesis (hourglass: fan-out → merge → review) ────────────────
    let v2 = if let Ok(cached) = ws.get_artifact("v2.rs") {
        eprintln!("==> V2 synthesis (resuming from checkpoint)");
        cached
    } else {
        eprintln!("==> V2 synthesis");
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
                args.max_iter,
                &toolbox,
            )
            .expect("synthesis failed");
        ws.emit_artifact("v2.rs", &v).unwrap();
        v
    };
    eprintln!("    v2 done");

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
