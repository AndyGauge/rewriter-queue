use crate::backend::{Backend, Submission};
use serde_json::{json, Value};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

fn tools() -> Value {
    json!([
        {
            "name": "queue_submit",
            "description": "Queue a synthesis run. Jobs run one at a time in submission order. Paths are on this machine; with a queue server configured, both directories are uploaded to it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "source": { "type": "string", "description": "Source directory to rewrite" },
                    "workspace": { "type": "string", "description": "Workspace directory for the run" },
                    "max_iter": { "type": "integer", "description": "Max review iterations per stage" },
                    "inherit_env": { "type": "boolean", "description": "Also use providers from the queue worker's shell environment (API keys); default is its config file only" }
                },
                "required": ["source", "workspace"]
            }
        },
        {
            "name": "queue_list",
            "description": "Show every job with its state, current stage and elapsed time.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "queue_status",
            "description": "Detailed status of one job, including the last lines of its log.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer" },
                    "log_lines": { "type": "integer", "description": "Log lines to include (default 20)" }
                },
                "required": ["id"]
            }
        },
        {
            "name": "queue_fetch",
            "description": "Download a job's whole uploaded workspace from the queue server into a local directory. Heavy — includes the project tree as submitted (node_modules, build output, etc). For just the orchestrator's own generated artifacts, use queue_artifacts / queue_artifact instead.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer" },
                    "out": { "type": "string", "description": "Local directory to write the workspace into" }
                },
                "required": ["id", "out"]
            }
        },
        {
            "name": "queue_download",
            "description": "Download everything a job has produced in one shot, like `cargo fetch` or `bundle install` — no need to list or pick individual files first. Pulls the job's whole artifacts/ tree (objective_contract.md, refined_test_matrix.json, inductive_analysis.md, schema.rs, the v2/ crate, synthesis/ checkpoints) into a local directory. Lighter than queue_fetch, which also drags in the originally-uploaded project tree (node_modules, .env, build output, etc).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer" },
                    "out": { "type": "string", "description": "Local directory to write the artifacts tree into" }
                },
                "required": ["id", "out"]
            }
        },
        {
            "name": "queue_artifacts",
            "description": "List the artifact files a job has emitted so far (objective_contract.md, refined_test_matrix.json, inductive_analysis.md, v2/Cargo.toml, ...). Works on a running job, not just a finished one.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "integer" } },
                "required": ["id"]
            }
        },
        {
            "name": "queue_artifact",
            "description": "Read one artifact a job has emitted, by the path queue_artifacts printed (e.g. \"objective_contract.md\"). Without `out`, returns the raw content inline. With `out`, instead downloads it to that local file path and returns a confirmation. Works on a running job, not just a finished one.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "integer" },
                    "path": { "type": "string", "description": "Artifact path relative to the job's artifacts/ dir" },
                    "out": { "type": "string", "description": "Local file path to save the artifact to, instead of returning its content inline" }
                },
                "required": ["id", "path"]
            }
        },
        {
            "name": "queue_cancel",
            "description": "Cancel a queued or running job.",
            "inputSchema": { "type": "object", "properties": { "id": { "type": "integer" } }, "required": ["id"] }
        }
    ])
}

fn call(backend: &Backend, name: &str, args: &Value) -> io::Result<String> {
    let id = || {
        args["id"]
            .as_u64()
            .map(|n| n as u32)
            .ok_or_else(|| io::Error::other("missing required field: id"))
    };
    let text = |field: &str| {
        args[field]
            .as_str()
            .ok_or_else(|| io::Error::other(format!("missing required field: {field}")))
    };
    match name {
        "queue_submit" => backend.submit(Submission {
            source: PathBuf::from(text("source")?),
            workspace: PathBuf::from(text("workspace")?),
            max_iter: args["max_iter"].as_u64().map(|n| n as u32),
            inherit_env: args["inherit_env"].as_bool().unwrap_or(false),
        }),
        "queue_list" => backend.list(),
        "queue_status" => {
            let lines = args["log_lines"].as_u64().unwrap_or(20) as usize;
            backend.status(id()?, lines)
        }
        "queue_fetch" => backend.fetch(id()?, Path::new(text("out")?)),
        "queue_download" => backend.download_artifacts(id()?, Path::new(text("out")?)),
        "queue_artifacts" => backend.list_artifacts(id()?),
        "queue_artifact" => {
            let content = backend.read_artifact(id()?, text("path")?)?;
            match args["out"].as_str() {
                Some(out) => {
                    if let Some(parent) = Path::new(out).parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(out, &content)?;
                    Ok(format!("wrote {} bytes to {out}", content.len()))
                }
                None => Ok(content),
            }
        }
        "queue_cancel" => backend.cancel(id()?),
        other => Err(io::Error::other(format!("unknown tool: {other}"))),
    }
}

fn handle(backend: &Backend, req: &Value) -> Value {
    let id = &req["id"];
    match req["method"].as_str().unwrap_or("") {
        "initialize" => json!({
            "jsonrpc": "2.0", "id": id,
            "result": {
                "protocolVersion": "2024-11-05",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "rewriter-queue", "version": "0.1.0" }
            }
        }),
        "tools/list" => json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": tools() } }),
        "tools/call" => {
            let params = &req["params"];
            let (text, is_error) = match call(
                backend,
                params["name"].as_str().unwrap_or(""),
                &params["arguments"],
            ) {
                Ok(t) => (t, false),
                Err(e) => (format!("Error: {e}"), true),
            };
            json!({
                "jsonrpc": "2.0", "id": id,
                "result": { "content": [{ "type": "text", "text": text }], "isError": is_error }
            })
        }
        m if m.starts_with("notifications/") => Value::Null,
        other => json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": -32601, "message": format!("Method not found: {other}") }
        }),
    }
}

pub fn serve(backend: &Backend) -> io::Result<()> {
    let mut out = io::stdout().lock();
    for line in io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let resp = match serde_json::from_str::<Value>(&line) {
            Ok(req) => handle(backend, &req),
            Err(e) => json!({
                "jsonrpc": "2.0", "id": Value::Null,
                "error": { "code": -32700, "message": format!("Parse error: {e}") }
            }),
        };
        if !resp.is_null() {
            writeln!(out, "{resp}")?;
            out.flush()?;
        }
    }
    Ok(())
}
