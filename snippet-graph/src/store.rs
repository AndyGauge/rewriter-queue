use crate::model::NodeId;
use crate::version::{NodeVersion, Results, Store, StoreError, VersionId, View, ViewId};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn io_err(e: std::io::Error) -> StoreError {
    StoreError::Io(e.to_string())
}

fn safe_name(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Directory-backed store: content-addressed JSON files under
/// `versions/`, `views/` and `results/`, plus an append-only `order.log`
/// recording creation order. Writes are atomic (temp file then rename) and
/// every read verifies the content hash matches the file name.
pub struct DirStore {
    root: PathBuf,
}

impl DirStore {
    pub fn open(root: &Path) -> Result<Self, StoreError> {
        for dir in ["versions", "views", "results"] {
            fs::create_dir_all(root.join(dir)).map_err(io_err)?;
        }
        Ok(DirStore {
            root: root.to_path_buf(),
        })
    }

    fn path(&self, dir: &str, id: &str) -> Result<PathBuf, StoreError> {
        if !safe_name(id) {
            return Err(StoreError::NotFound(id.to_string()));
        }
        Ok(self.root.join(dir).join(format!("{id}.json")))
    }

    fn write_atomic<T: Serialize>(&self, path: &Path, value: &T) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec_pretty(value).map_err(|e| StoreError::Io(e.to_string()))?;
        let tmp = self.root.join(format!(
            ".tmp-{}-{}",
            std::process::id(),
            TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let written = fs::File::create(&tmp)
            .and_then(|mut f| f.write_all(&bytes).and_then(|_| f.sync_all()))
            .and_then(|_| fs::rename(&tmp, path));
        if let Err(e) = written {
            let _ = fs::remove_file(&tmp);
            return Err(io_err(e));
        }
        Ok(())
    }

    fn read_json<T: DeserializeOwned>(&self, path: &Path, id: &str) -> Result<T, StoreError> {
        let bytes = match fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == ErrorKind::NotFound => {
                return Err(StoreError::NotFound(id.to_string()))
            }
            Err(e) => return Err(io_err(e)),
        };
        serde_json::from_slice(&bytes).map_err(|e| StoreError::Corrupt(format!("{id}: {e}")))
    }

    fn append_order(&self, entry: serde_json::Value) -> Result<(), StoreError> {
        let mut line = entry.to_string();
        line.push('\n');
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.root.join("order.log"))
            .and_then(|mut f| f.write_all(line.as_bytes()))
            .map_err(io_err)
    }

    fn order_entries(&self) -> Result<Vec<serde_json::Value>, StoreError> {
        let text = match fs::read_to_string(self.root.join("order.log")) {
            Ok(t) => t,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(io_err(e)),
        };
        text.lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| {
                serde_json::from_str(l).map_err(|e| StoreError::Corrupt(format!("order.log: {e}")))
            })
            .collect()
    }
}

impl Store for DirStore {
    fn put_version(&mut self, version: NodeVersion) -> Result<VersionId, StoreError> {
        let id = version.id.clone();
        if NodeVersion::new(
            &version.to_node(),
            version.parent.clone(),
            version.origin.clone(),
        )
        .id != id
        {
            return Err(StoreError::Corrupt(format!(
                "{}: id does not match content",
                id.0
            )));
        }
        let path = self.path("versions", &id.0)?;
        if path.exists() {
            return Ok(id);
        }
        self.write_atomic(&path, &version)?;
        self.append_order(
            serde_json::json!({"kind": "version", "node": version.node.0, "id": id.0}),
        )?;
        Ok(id)
    }

    fn get_version(&self, id: &VersionId) -> Result<NodeVersion, StoreError> {
        let version: NodeVersion = self.read_json(&self.path("versions", &id.0)?, &id.0)?;
        let recomputed = NodeVersion::new(
            &version.to_node(),
            version.parent.clone(),
            version.origin.clone(),
        );
        if version.id != *id || recomputed.id != *id {
            return Err(StoreError::Corrupt(format!("{}: hash mismatch", id.0)));
        }
        Ok(version)
    }

    fn versions_of(&self, node: &NodeId) -> Result<Vec<VersionId>, StoreError> {
        let mut out: Vec<VersionId> = Vec::new();
        for entry in self.order_entries()? {
            if entry["kind"] == "version" && entry["node"] == node.0.as_str() {
                if let Some(id) = entry["id"].as_str() {
                    let id = VersionId(id.to_string());
                    if !out.contains(&id) {
                        out.push(id);
                    }
                }
            }
        }
        Ok(out)
    }

    fn put_view(&mut self, view: View) -> Result<ViewId, StoreError> {
        let id = view.id.clone();
        let recomputed = View::new(
            view.run.clone(),
            view.parent.clone(),
            view.versions.clone(),
            view.uses.clone(),
            view.created_ms,
        );
        if recomputed.id != id {
            return Err(StoreError::Corrupt(format!(
                "{}: id does not match content",
                id.0
            )));
        }
        let path = self.path("views", &id.0)?;
        if path.exists() {
            return Ok(id);
        }
        self.write_atomic(&path, &view)?;
        self.append_order(serde_json::json!({"kind": "view", "id": id.0}))?;
        Ok(id)
    }

    fn get_view(&self, id: &ViewId) -> Result<View, StoreError> {
        let view: View = self.read_json(&self.path("views", &id.0)?, &id.0)?;
        let recomputed = View::new(
            view.run.clone(),
            view.parent.clone(),
            view.versions.clone(),
            view.uses.clone(),
            view.created_ms,
        );
        if view.id != *id || recomputed.id != *id {
            return Err(StoreError::Corrupt(format!("{}: hash mismatch", id.0)));
        }
        Ok(view)
    }

    fn list_views(&self) -> Result<Vec<ViewId>, StoreError> {
        let mut out: Vec<ViewId> = Vec::new();
        for entry in self.order_entries()? {
            if entry["kind"] == "view" {
                if let Some(id) = entry["id"].as_str() {
                    let id = ViewId(id.to_string());
                    if !out.contains(&id) {
                        out.push(id);
                    }
                }
            }
        }
        Ok(out)
    }

    fn put_results(&mut self, results: Results) -> Result<(), StoreError> {
        let path = self
            .path("results", &results.view.0)
            .map_err(|_| StoreError::Io(format!("invalid view id: {}", results.view.0)))?;
        self.write_atomic(&path, &results)
    }

    fn results_for(&self, view: &ViewId) -> Result<Option<Results>, StoreError> {
        let Ok(path) = self.path("results", &view.0) else {
            return Ok(None);
        };
        match self.read_json::<Results>(&path, &view.0) {
            Ok(r) if r.view == *view => Ok(Some(r)),
            Ok(_) => Err(StoreError::Corrupt(format!("{}: view mismatch", view.0))),
            Err(StoreError::NotFound(_)) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn all_results(&self) -> Result<Vec<Results>, StoreError> {
        let mut names: Vec<String> = Vec::new();
        for entry in fs::read_dir(self.root.join("results")).map_err(io_err)? {
            let name = entry
                .map_err(io_err)?
                .file_name()
                .to_string_lossy()
                .into_owned();
            if let Some(stem) = name.strip_suffix(".json") {
                names.push(stem.to_string());
            }
        }
        names.sort();
        let mut out = Vec::new();
        for name in names {
            if let Some(r) = self.results_for(&ViewId(name))? {
                out.push(r);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Node, NodeKind, Placement};
    use crate::version::{Origin, RunId};
    use std::collections::BTreeMap;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "sg-store-{}-{}",
                std::process::id(),
                TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&dir);
            TempDir(dir)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn origin() -> Origin {
        Origin {
            run: RunId("r1".into()),
            model: "m".into(),
            prompt_hash: "p".into(),
            created_ms: 1,
        }
    }

    fn node(name: &str, body: &str) -> Node {
        Node {
            id: NodeId(name.into()),
            kind: NodeKind::Function,
            signature: format!("fn {name}()"),
            body: Some(body.into()),
            impl_header: None,
            placement: Placement {
                file: "lib.rs".into(),
                index: 0,
            },
        }
    }

    fn view(parent: Option<ViewId>, version: &str, ms: u64) -> View {
        View::new(
            RunId("r1".into()),
            parent,
            BTreeMap::from([(NodeId("f".into()), VersionId(version.into()))]),
            BTreeMap::new(),
            ms,
        )
    }

    fn results(view: &ViewId, recorded_ms: u64) -> Results {
        Results {
            view: view.clone(),
            tests: Vec::new(),
            benches: Vec::new(),
            clippy_clean: Some(true),
            recorded_ms,
        }
    }

    #[test]
    fn version_round_trips() {
        let dir = TempDir::new();
        let mut s = DirStore::open(&dir.0).unwrap();
        let v = NodeVersion::new(&node("f", "{ 1 }"), None, origin());
        let id = s.put_version(v.clone()).unwrap();
        assert_eq!(s.get_version(&id).unwrap(), v);
        assert_eq!(
            s.get_version(&VersionId("nope".into())),
            Err(StoreError::NotFound("nope".into()))
        );
    }

    #[test]
    fn put_dedupes_and_keeps_creation_order() {
        let dir = TempDir::new();
        let mut s = DirStore::open(&dir.0).unwrap();
        let a = s
            .put_version(NodeVersion::new(&node("f", "{ 1 }"), None, origin()))
            .unwrap();
        let c = s
            .put_version(NodeVersion::new(
                &node("f", "{ 2 }"),
                Some(a.clone()),
                origin(),
            ))
            .unwrap();
        let a2 = s
            .put_version(NodeVersion::new(&node("f", "{ 1 }"), None, origin()))
            .unwrap();
        s.put_version(NodeVersion::new(&node("g", "{ 3 }"), None, origin()))
            .unwrap();
        assert_eq!(a, a2);
        assert_eq!(s.versions_of(&NodeId("f".into())).unwrap(), vec![a, c]);
        assert!(s.versions_of(&NodeId("zzz".into())).unwrap().is_empty());
    }

    #[test]
    fn views_round_trip_in_order_without_duplicates() {
        let dir = TempDir::new();
        let mut s = DirStore::open(&dir.0).unwrap();
        let v1 = view(None, "a", 1);
        let v2 = view(Some(v1.id.clone()), "b", 2);
        s.put_view(v1.clone()).unwrap();
        s.put_view(v2.clone()).unwrap();
        s.put_view(v1.clone()).unwrap();
        assert_eq!(s.get_view(&v2.id).unwrap(), v2);
        assert_eq!(s.list_views().unwrap(), vec![v1.id, v2.id]);
    }

    #[test]
    fn results_overwrite_by_view() {
        let dir = TempDir::new();
        let mut s = DirStore::open(&dir.0).unwrap();
        let v1 = view(None, "a", 1);
        let v2 = view(None, "b", 2);
        assert_eq!(s.results_for(&v1.id).unwrap(), None);
        s.put_results(results(&v1.id, 1)).unwrap();
        s.put_results(results(&v2.id, 2)).unwrap();
        s.put_results(results(&v1.id, 3)).unwrap();
        assert_eq!(s.results_for(&v1.id).unwrap().unwrap().recorded_ms, 3);
        assert_eq!(s.all_results().unwrap().len(), 2);
    }

    #[test]
    fn corrupted_files_are_detected() {
        let dir = TempDir::new();
        let mut s = DirStore::open(&dir.0).unwrap();
        let id = s
            .put_version(NodeVersion::new(&node("f", "{ 1 }"), None, origin()))
            .unwrap();
        let path = dir.0.join("versions").join(format!("{}.json", id.0));
        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, text.replace("{ 1 }", "{ 2 }")).unwrap();
        assert!(matches!(s.get_version(&id), Err(StoreError::Corrupt(_))));
        fs::write(&path, "not json").unwrap();
        assert!(matches!(s.get_version(&id), Err(StoreError::Corrupt(_))));

        let v = view(None, "a", 1);
        s.put_view(v.clone()).unwrap();
        let path = dir.0.join("views").join(format!("{}.json", v.id.0));
        let text = fs::read_to_string(&path).unwrap();
        fs::write(&path, text.replace("\"a\"", "\"b\"")).unwrap();
        assert!(matches!(s.get_view(&v.id), Err(StoreError::Corrupt(_))));
    }

    #[test]
    fn reopening_sees_prior_data() {
        let dir = TempDir::new();
        let (id, v) = {
            let mut s = DirStore::open(&dir.0).unwrap();
            let id = s
                .put_version(NodeVersion::new(&node("f", "{ 1 }"), None, origin()))
                .unwrap();
            let v = view(None, "a", 1);
            s.put_view(v.clone()).unwrap();
            s.put_results(results(&v.id, 5)).unwrap();
            (id, v)
        };
        let s = DirStore::open(&dir.0).unwrap();
        assert_eq!(s.versions_of(&NodeId("f".into())).unwrap(), vec![id]);
        assert_eq!(s.list_views().unwrap(), vec![v.id.clone()]);
        assert_eq!(s.results_for(&v.id).unwrap().unwrap().recorded_ms, 5);
    }

    #[test]
    fn two_stores_writing_same_content_do_not_conflict() {
        let dir = TempDir::new();
        let mut a = DirStore::open(&dir.0).unwrap();
        let mut b = DirStore::open(&dir.0).unwrap();
        let ia = a
            .put_version(NodeVersion::new(&node("f", "{ 1 }"), None, origin()))
            .unwrap();
        let ib = b
            .put_version(NodeVersion::new(&node("f", "{ 1 }"), None, origin()))
            .unwrap();
        assert_eq!(ia, ib);
        let v = view(None, "a", 1);
        a.put_view(v.clone()).unwrap();
        b.put_view(v.clone()).unwrap();
        assert_eq!(a.versions_of(&NodeId("f".into())).unwrap(), vec![ia]);
        assert_eq!(b.list_views().unwrap(), vec![v.id]);
    }
}
