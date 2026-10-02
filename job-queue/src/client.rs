use crate::archive;
use crate::events::{self, EventFilter};
use run_events::{parse_events, Analysis, Event, FeatureRequest, RequestStatus};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::Duration;

pub struct Client {
    agent: ureq::Agent,
    base: String,
    token: String,
}

impl Client {
    pub fn new(base: String, token: String) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout(Duration::from_secs(120))
            .build();
        Self { agent, base, token }
    }

    fn request(&self, method: &str, path: &str) -> ureq::Request {
        self.agent
            .request(method, &format!("{}{path}", self.base))
            .set("authorization", &format!("Bearer {}", self.token))
    }

    fn send(&self, result: Result<ureq::Response, ureq::Error>) -> io::Result<ureq::Response> {
        result.map_err(|e| match e {
            ureq::Error::Status(code, resp) => io::Error::other(format!(
                "queue server replied {code}: {}",
                resp.into_string().unwrap_or_default().trim()
            )),
            ureq::Error::Transport(t) => {
                io::Error::other(format!("cannot reach queue server at {}: {t}", self.base))
            }
        })
    }

    pub fn get(&self, path: &str) -> io::Result<String> {
        self.send(self.request("GET", path).call())?.into_string()
    }

    pub fn get_query(&self, path: &str, params: &[(&str, &str)]) -> io::Result<String> {
        let mut req = self.request("GET", path);
        for (k, v) in params {
            req = req.query(k, v);
        }
        self.send(req.call())?.into_string()
    }

    pub fn post(&self, path: &str) -> io::Result<String> {
        self.send(self.request("POST", path).call())?.into_string()
    }

    pub fn events(&self, id: u32, filter: &EventFilter) -> io::Result<Vec<Event>> {
        let kind = filter.kind.map(events::kind_label);
        let mut params: Vec<(&str, &str)> = Vec::new();
        if let Some(k) = &kind {
            params.push(("kind", k));
        }
        if let Some(a) = &filter.agent {
            params.push(("agent", a));
        }
        if let Some(s) = &filter.since {
            params.push(("since", s));
        }
        let text = self.get_query(&format!("/jobs/{id}/events"), &params)?;
        Ok(parse_events(&text))
    }

    pub fn analysis(&self, id: u32) -> io::Result<Analysis> {
        let text = self.get(&format!("/jobs/{id}/analysis"))?;
        serde_json::from_str(&text).map_err(io::Error::other)
    }

    pub fn requests(&self) -> io::Result<Vec<FeatureRequest>> {
        let text = self.get("/feature-requests")?;
        serde_json::from_str(&text).map_err(io::Error::other)
    }

    pub fn set_request_status(
        &self,
        id: &str,
        status: RequestStatus,
    ) -> io::Result<FeatureRequest> {
        let body = serde_json::json!({ "status": status });
        let text = self
            .send(
                self.request("POST", &format!("/feature-requests/{id}/status"))
                    .send_json(body),
            )?
            .into_string()?;
        serde_json::from_str(&text).map_err(io::Error::other)
    }

    pub fn submit(
        &self,
        source: &Path,
        workspace: &Path,
        max_iter: Option<u32>,
        inherit_env: bool,
    ) -> io::Result<String> {
        let manifest = workspace_manifest(source);
        let mut parts: Vec<(&Path, &str)> = vec![(source, "source"), (workspace, "ws")];
        if let Some(m) = &manifest {
            parts.push((m, "Cargo.toml"));
        }
        let body = archive::pack(&parts)?;

        let name = source
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut request = self
            .request("POST", "/jobs")
            .query("name", &name)
            .query("inherit_env", &inherit_env.to_string());
        if let Some(n) = max_iter {
            request = request.query("max_iter", &n.to_string());
        }
        self.send(request.send_bytes(&body))?.into_string()
    }

    pub fn fetch(&self, id: u32, out: &Path) -> io::Result<String> {
        let resp = self.send(self.request("GET", &format!("/jobs/{id}/result")).call())?;
        let mut bytes = Vec::new();
        resp.into_reader().read_to_end(&mut bytes)?;
        archive::unpack(&bytes, out)?;
        Ok(format!(
            "workspace for job #{id} written to {}",
            out.display()
        ))
    }

    pub fn download_artifacts(&self, id: u32, out: &Path) -> io::Result<String> {
        let resp = self.send(
            self.request("GET", &format!("/jobs/{id}/artifacts.tar.gz"))
                .call(),
        )?;
        let mut bytes = Vec::new();
        resp.into_reader().read_to_end(&mut bytes)?;
        archive::unpack(&bytes, out)?;
        Ok(format!(
            "artifacts for job #{id} written to {}",
            out.display()
        ))
    }
}

/// Mirrors the synthesis program's lookup: the nearest ancestor Cargo.toml that declares a
/// [workspace], which it needs to resolve inherited dependency versions.
fn workspace_manifest(source: &Path) -> Option<PathBuf> {
    source
        .ancestors()
        .skip(1)
        .map(|d| d.join("Cargo.toml"))
        .find(|candidate| {
            std::fs::read_to_string(candidate).is_ok_and(|t| t.contains("[workspace]"))
        })
}
