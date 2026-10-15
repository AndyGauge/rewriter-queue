use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

const POSTGRES_VERSION: &str = "18.6.0";

/// Service names a mission declares with a `Services: postgres` line.
pub(crate) fn declared_services(mission: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in mission.lines() {
        let line = line.trim().trim_start_matches(['-', '*', ' ']).trim_start();
        let lower = line.to_lowercase();
        let Some(rest) = lower.strip_prefix("services:") else {
            continue;
        };
        for name in rest.split(',') {
            let name = name.trim().trim_matches(['*', '`', '.']).trim().to_string();
            if !name.is_empty() && !out.contains(&name) {
                out.push(name);
            }
        }
    }
    out
}

fn release_triple() -> Option<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some("x86_64-unknown-linux-gnu"),
        ("linux", "aarch64") => Some("aarch64-unknown-linux-gnu"),
        ("macos", "x86_64") => Some("x86_64-apple-darwin"),
        ("macos", "aarch64") => Some("aarch64-apple-darwin"),
        _ => None,
    }
}

fn cache_root() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(std::env::temp_dir);
    base.join("rewriter/postgres")
}

fn free_port() -> std::io::Result<u16> {
    Ok(std::net::TcpListener::bind("127.0.0.1:0")?.local_addr()?.port())
}

fn run(cmd: &mut Command) -> Result<String, String> {
    let out = cmd.output().map_err(|e| format!("{cmd:?}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!("{cmd:?} failed: {}", String::from_utf8_lossy(&out.stderr).trim()))
    }
}

/// Postgres binaries, downloaded once into the user cache. Returns the `bin` directory.
fn ensure_binaries() -> Result<PathBuf, String> {
    let triple = release_triple().ok_or("no prebuilt Postgres for this platform")?;
    let dir = cache_root().join(POSTGRES_VERSION);
    let bin = dir.join("bin");
    if bin.join("pg_ctl").exists() {
        return Ok(bin);
    }
    let staging = cache_root().join(format!("{POSTGRES_VERSION}.partial-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    let url = format!(
        "https://github.com/theseus-rs/postgresql-binaries/releases/download/{POSTGRES_VERSION}/postgresql-{POSTGRES_VERSION}-{triple}.tar.gz"
    );
    let archive = staging.join("pg.tar.gz");
    run(Command::new("curl").args(["-sfL", "-o"]).arg(&archive).arg(&url))?;
    run(Command::new("tar").arg("xzf").arg(&archive).arg("-C").arg(&staging).arg("--strip-components=1"))?;
    if !staging.join("bin/pg_ctl").exists() {
        return Err("downloaded Postgres archive has no bin/pg_ctl".into());
    }
    let _ = std::fs::remove_file(&archive);
    if std::fs::rename(&staging, &dir).is_err() {
        let _ = std::fs::remove_dir_all(&staging);
        if !bin.join("pg_ctl").exists() {
            return Err("could not install the Postgres binaries".into());
        }
    }
    Ok(bin)
}

/// A throwaway Postgres server in a temp directory on a free localhost port. It is stopped
/// and deleted by `stop`, and by a watchdog if the orchestrator is killed before it can.
pub(crate) struct Postgres {
    bin: PathBuf,
    root: PathBuf,
    port: u16,
    next_db: AtomicU32,
}

/// A database inside the throwaway server, dropped when this goes out of scope.
pub(crate) struct Database {
    server: &'static Postgres,
    name: String,
}

impl Database {
    pub(crate) fn url(&self) -> String {
        self.server.url(&self.name)
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        let _ = self.server.psql(&format!("DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)", self.name));
    }
}

impl Postgres {
    fn start() -> Result<Self, String> {
        let bin = ensure_binaries()?;
        let root = std::env::temp_dir().join(format!("rewriter-pg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).map_err(|e| e.to_string())?;
        let data = root.join("data");
        run(Command::new(bin.join("initdb"))
            .arg("-D").arg(&data).args(["-U", "postgres", "--auth=trust", "-E", "UTF8"]))?;
        let port = free_port().map_err(|e| e.to_string())?;
        let opts = format!("-p {port} -k {} -c listen_addresses=127.0.0.1 -c fsync=off", root.display());
        run(Command::new(bin.join("pg_ctl"))
            .arg("-D").arg(&data).arg("-o").arg(opts)
            .arg("-l").arg(root.join("postgres.log")).args(["-w", "start"]))?;
        let server = Self { bin, root, port, next_db: AtomicU32::new(0) };
        server.spawn_watchdog();
        Ok(server)
    }

    /// Outlives a SIGKILL of the orchestrator: waits for its pid to disappear, then cleans up.
    fn spawn_watchdog(&self) {
        let script = format!(
            "while kill -0 {pid} 2>/dev/null; do sleep 5; done; \
             '{pg_ctl}' -D '{data}' -m immediate stop >/dev/null 2>&1; rm -rf '{root}'",
            pid = std::process::id(),
            pg_ctl = self.bin.join("pg_ctl").display(),
            data = self.root.join("data").display(),
            root = self.root.display(),
        );
        let _ = Command::new("sh")
            .args(["-c", &script])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }

    fn url(&self, db: &str) -> String {
        format!("postgres://postgres@127.0.0.1:{}/{db}", self.port)
    }

    fn psql(&self, sql: &str) -> Result<String, String> {
        run(Command::new(self.bin.join("psql"))
            .args(["-h", "127.0.0.1", "-p", &self.port.to_string(), "-U", "postgres", "-d", "postgres", "-Atc", sql]))
    }

    /// A fresh empty database, so quality gates running in parallel never share state.
    pub(crate) fn new_database(&'static self) -> Result<Database, String> {
        let name = format!("gate_{}", self.next_db.fetch_add(1, Ordering::Relaxed));
        self.psql(&format!("CREATE DATABASE \"{name}\""))?;
        Ok(Database { server: self, name })
    }

    fn stop(&self) {
        let _ = Command::new(self.bin.join("pg_ctl"))
            .arg("-D").arg(self.root.join("data")).args(["-m", "immediate", "stop"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

static POSTGRES: OnceLock<Postgres> = OnceLock::new();
static STOPPED: Mutex<bool> = Mutex::new(false);

/// Start whatever services the mission declares. Unknown names are reported, not fatal.
pub(crate) fn start_declared(mission: &str) -> Result<(), String> {
    for name in declared_services(mission) {
        match name.as_str() {
            "postgres" | "postgresql" => {
                eprintln!("Services: starting a throwaway Postgres {POSTGRES_VERSION} (DATABASE_URL is set for tests)");
                let server = Postgres::start()?;
                eprintln!("Services: Postgres listening on 127.0.0.1:{}", server.port);
                let _ = POSTGRES.set(server);
            }
            other => eprintln!("Services: '{other}' is not a supported service -- ignored"),
        }
    }
    Ok(())
}

pub(crate) fn postgres() -> Option<&'static Postgres> {
    POSTGRES.get()
}

/// Stop every service started by `start_declared`. Safe to call more than once.
pub(crate) fn stop_all() {
    let mut stopped = STOPPED.lock().unwrap();
    if *stopped {
        return;
    }
    *stopped = true;
    if let Some(server) = POSTGRES.get() {
        server.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_services_line_names_the_services() {
        assert_eq!(declared_services("# Mission\n\nServices: postgres\n"), vec!["postgres"]);
    }

    #[test]
    fn bullets_bold_commas_and_case_are_tolerated() {
        assert_eq!(declared_services("- **Services:** Postgres, Redis."), vec!["postgres", "redis"]);
        assert_eq!(declared_services("  services: postgres,postgres"), vec!["postgres"]);
    }

    #[test]
    fn prose_that_merely_mentions_postgres_declares_nothing() {
        assert!(declared_services("Backed by PostgreSQL via sqlx.").is_empty());
    }

    #[test]
    #[ignore = "downloads Postgres; run with --ignored"]
    fn a_gate_database_is_usable_and_gone_after_stop() {
        start_declared("Services: postgres").unwrap();
        let pg = postgres().unwrap();
        let root = pg.root.clone();
        {
            let db = pg.new_database().unwrap();
            assert!(db.url().starts_with("postgres://postgres@127.0.0.1:"));
            let out = run(Command::new(pg.bin.join("psql"))
                .args(["-h", "127.0.0.1", "-p", &pg.port.to_string(), "-U", "postgres", "-d"])
                .arg(&db.name)
                .args(["-Atc", "create table t(a int); insert into t values (7); select a from t"]))
            .unwrap();
            assert!(out.contains('7'));
            let second = pg.new_database().unwrap();
            assert_ne!(db.url(), second.url());
        }
        assert!(!pg.psql("select datname from pg_database").unwrap().contains("gate_"));
        stop_all();
        assert!(!root.exists());
    }
}
