mod archive;
mod backend;
mod client;
mod config;
mod job;
mod mcp;
mod server;
mod view;
mod worker;

use backend::{Backend, Submission};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

fn usage() -> ! {
    eprintln!(
        "Usage: rewriter-queue <command>

Commands:
  submit --source <dir> --workspace <dir> [--max-iter <n>] [--inherit-env]
                    Queue a synthesis run
  list              Show all jobs
  status <id>       Show one job with the tail of its log
  cancel <id>       Cancel a queued or running job
  fetch <id> --out <dir>
                    Download a finished job's workspace from the queue server
  download <id> --out <dir>
                    Download everything a job has emitted so far (the whole artifacts/
                    tree) in one shot — works while running, lighter than fetch
  artifacts <id>    List artifact files a job has emitted so far (works while running)
  artifact <id> <path>
                    Print one artifact's content (works while running)
  watch             Live queue view; refreshes until interrupted
  serve [--bind <addr>]
                    Host the queue over HTTP (default 0.0.0.0:8003) and run its jobs
  worker            Run a local worker in the foreground
  mcp               Serve the queue over MCP on stdio

With a queue server configured ([queue] url in config.toml, or REWRITER_QUEUE_URL),
every command talks to that server; otherwise a local queue is used.
The token comes from REWRITER_QUEUE_TOKEN or queue-token beside config.toml.
Jobs use only the worker's config file for providers unless --inherit-env is given.
Local queue data lives in $REWRITER_QUEUE_DIR (default ~/.local/share/rewriter/queue)."
    );
    std::process::exit(2);
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|w| w[0] == name)
        .map(|w| w[1].as_str())
}

fn id_arg(args: &[String]) -> u32 {
    args.first()
        .and_then(|s| s.trim_start_matches('#').parse().ok())
        .unwrap_or_else(|| usage())
}

fn local_queue() -> std::io::Result<job::Queue> {
    job::Queue::open()
}

fn run(args: &[String]) -> std::io::Result<()> {
    let rest = args.get(1..).unwrap_or(&[]);
    match args.first().map(String::as_str) {
        Some("submit") => {
            let (Some(source), Some(workspace)) =
                (flag(rest, "--source"), flag(rest, "--workspace"))
            else {
                usage()
            };
            let text = Backend::open()?.submit(Submission {
                source: PathBuf::from(source),
                workspace: PathBuf::from(workspace),
                max_iter: flag(rest, "--max-iter").and_then(|n| n.parse().ok()),
                inherit_env: rest.iter().any(|a| a == "--inherit-env"),
            })?;
            println!("{text}");
        }
        Some("list") => println!("{}", Backend::open()?.list()?),
        Some("status") => println!("{}", Backend::open()?.status(id_arg(rest), 20)?),
        Some("cancel") => println!("{}", Backend::open()?.cancel(id_arg(rest))?),
        Some("fetch") => {
            let Some(out) = flag(rest, "--out") else {
                usage()
            };
            println!("{}", Backend::open()?.fetch(id_arg(rest), Path::new(out))?);
        }
        Some("download") => {
            let Some(out) = flag(rest, "--out") else {
                usage()
            };
            println!(
                "{}",
                Backend::open()?.download_artifacts(id_arg(rest), Path::new(out))?
            );
        }
        Some("artifacts") => println!("{}", Backend::open()?.list_artifacts(id_arg(rest))?),
        Some("artifact") => {
            let id = id_arg(rest);
            let Some(path) = rest.get(1) else { usage() };
            println!("{}", Backend::open()?.read_artifact(id, path)?);
        }
        Some("watch") => {
            let backend = Backend::open()?;
            loop {
                let frame = backend
                    .watch_frame()
                    .unwrap_or_else(|e| format!("{e}\n\nretrying…"));
                print!("\x1b[2J\x1b[H{frame}");
                std::io::stdout().flush()?;
                std::thread::sleep(Duration::from_secs(1));
            }
        }
        Some("serve") => {
            let bind = flag(rest, "--bind").unwrap_or("0.0.0.0:8003");
            server::run(local_queue()?, bind, config::token()?)?
        }
        Some("worker") => worker::run(&local_queue()?)?,
        Some("mcp") => mcp::serve(&Backend::open()?)?,
        _ => usage(),
    }
    Ok(())
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&args) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
