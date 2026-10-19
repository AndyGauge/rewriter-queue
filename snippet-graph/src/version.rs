use crate::model::{Node, NodeId, NodeKind, Placement, TestOutcome};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct VersionId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ViewId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RunId(pub String);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub run: RunId,
    pub model: String,
    pub prompt_hash: String,
    pub created_ms: u64,
}

/// One stored implementation of a node. The id hashes kind, signature, body and
/// impl header only, so identical content is one version regardless of origin.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeVersion {
    pub id: VersionId,
    pub node: NodeId,
    pub kind: NodeKind,
    pub signature: String,
    pub body: Option<String>,
    pub impl_header: Option<String>,
    pub placement: Placement,
    pub parent: Option<VersionId>,
    pub origin: Origin,
}

/// A chosen version for every node plus the edges between them: the state a
/// crate is rendered from. The id hashes the contents and the parent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct View {
    pub id: ViewId,
    pub run: RunId,
    pub parent: Option<ViewId>,
    pub versions: BTreeMap<NodeId, VersionId>,
    pub uses: BTreeMap<NodeId, BTreeSet<NodeId>>,
    pub created_ms: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BenchResult {
    pub bench: NodeId,
    pub median_ns: u64,
    pub spread_ns: u64,
    pub runs: u32,
    pub instructions: Option<u64>,
}

/// What was measured for one view. Kept apart from versions because a version's
/// behaviour depends on the rest of the view.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Results {
    pub view: ViewId,
    pub tests: Vec<TestOutcome>,
    pub benches: Vec<BenchResult>,
    pub clippy_clean: Option<bool>,
    pub recorded_ms: u64,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum StoreError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("corrupt: {0}")]
    Corrupt(String),
    #[error("io: {0}")]
    Io(String),
}

pub trait Store {
    fn put_version(&mut self, version: NodeVersion) -> Result<VersionId, StoreError>;
    fn get_version(&self, id: &VersionId) -> Result<NodeVersion, StoreError>;
    /// Versions of a node in creation order.
    fn versions_of(&self, node: &NodeId) -> Result<Vec<VersionId>, StoreError>;
    fn put_view(&mut self, view: View) -> Result<ViewId, StoreError>;
    fn get_view(&self, id: &ViewId) -> Result<View, StoreError>;
    /// Views in creation order.
    fn list_views(&self) -> Result<Vec<ViewId>, StoreError>;
    fn put_results(&mut self, results: Results) -> Result<(), StoreError>;
    fn results_for(&self, view: &ViewId) -> Result<Option<Results>, StoreError>;
    fn all_results(&self) -> Result<Vec<Results>, StoreError>;
}

/// Stable 128-bit FNV-1a over length-prefixed parts, as 32 hex digits.
pub fn content_hash(parts: &[&str]) -> String {
    const OFFSET: u128 = 0x6c62272e07bb014262b821756295c58d;
    const PRIME: u128 = 0x0000000001000000000000000000013b;
    let mut h = OFFSET;
    for part in parts {
        for byte in (part.len() as u64)
            .to_le_bytes()
            .iter()
            .chain(part.as_bytes())
        {
            h ^= u128::from(*byte);
            h = h.wrapping_mul(PRIME);
        }
    }
    format!("{h:032x}")
}

impl NodeVersion {
    pub fn new(node: &Node, parent: Option<VersionId>, origin: Origin) -> Self {
        let kind = format!("{:?}", node.kind);
        let id = VersionId(content_hash(&[
            &kind,
            &node.signature,
            node.body.as_deref().unwrap_or("\0none"),
            node.impl_header.as_deref().unwrap_or("\0none"),
        ]));
        NodeVersion {
            id,
            node: node.id.clone(),
            kind: node.kind,
            signature: node.signature.clone(),
            body: node.body.clone(),
            impl_header: node.impl_header.clone(),
            placement: node.placement.clone(),
            parent,
            origin,
        }
    }

    pub fn to_node(&self) -> Node {
        Node {
            id: self.node.clone(),
            kind: self.kind,
            signature: self.signature.clone(),
            body: self.body.clone(),
            impl_header: self.impl_header.clone(),
            placement: self.placement.clone(),
        }
    }
}

impl View {
    pub fn new(
        run: RunId,
        parent: Option<ViewId>,
        versions: BTreeMap<NodeId, VersionId>,
        uses: BTreeMap<NodeId, BTreeSet<NodeId>>,
        created_ms: u64,
    ) -> Self {
        let mut parts: Vec<String> = vec![parent.as_ref().map_or(String::new(), |p| p.0.clone())];
        for (node, version) in &versions {
            parts.push(format!("{}={}", node.0, version.0));
        }
        for (from, tos) in &uses {
            for to in tos {
                parts.push(format!("{}->{}", from.0, to.0));
            }
        }
        let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
        View {
            id: ViewId(content_hash(&refs)),
            run,
            parent,
            versions,
            uses,
            created_ms,
        }
    }
}

#[derive(Default)]
pub struct MemoryStore {
    versions: BTreeMap<VersionId, NodeVersion>,
    by_node: BTreeMap<NodeId, Vec<VersionId>>,
    views: BTreeMap<ViewId, View>,
    view_order: Vec<ViewId>,
    results: BTreeMap<ViewId, Results>,
}

impl Store for MemoryStore {
    fn put_version(&mut self, version: NodeVersion) -> Result<VersionId, StoreError> {
        let id = version.id.clone();
        if !self.versions.contains_key(&id) {
            self.by_node
                .entry(version.node.clone())
                .or_default()
                .push(id.clone());
            self.versions.insert(id.clone(), version);
        }
        Ok(id)
    }

    fn get_version(&self, id: &VersionId) -> Result<NodeVersion, StoreError> {
        self.versions
            .get(id)
            .cloned()
            .ok_or_else(|| StoreError::NotFound(id.0.clone()))
    }

    fn versions_of(&self, node: &NodeId) -> Result<Vec<VersionId>, StoreError> {
        Ok(self.by_node.get(node).cloned().unwrap_or_default())
    }

    fn put_view(&mut self, view: View) -> Result<ViewId, StoreError> {
        let id = view.id.clone();
        if !self.views.contains_key(&id) {
            self.view_order.push(id.clone());
            self.views.insert(id.clone(), view);
        }
        Ok(id)
    }

    fn get_view(&self, id: &ViewId) -> Result<View, StoreError> {
        self.views
            .get(id)
            .cloned()
            .ok_or_else(|| StoreError::NotFound(id.0.clone()))
    }

    fn list_views(&self) -> Result<Vec<ViewId>, StoreError> {
        Ok(self.view_order.clone())
    }

    fn put_results(&mut self, results: Results) -> Result<(), StoreError> {
        self.results.insert(results.view.clone(), results);
        Ok(())
    }

    fn results_for(&self, view: &ViewId) -> Result<Option<Results>, StoreError> {
        Ok(self.results.get(view).cloned())
    }

    fn all_results(&self) -> Result<Vec<Results>, StoreError> {
        Ok(self.results.values().cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn origin() -> Origin {
        Origin {
            run: RunId("r1".into()),
            model: "m".into(),
            prompt_hash: "p".into(),
            created_ms: 1,
        }
    }

    fn node(body: &str) -> Node {
        Node {
            id: NodeId("f".into()),
            kind: NodeKind::Function,
            signature: "fn f()".into(),
            body: Some(body.into()),
            impl_header: None,
            placement: Placement {
                file: "lib.rs".into(),
                index: 0,
            },
        }
    }

    #[test]
    fn identical_content_is_one_version() {
        let mut s = MemoryStore::default();
        let a = s
            .put_version(NodeVersion::new(&node("{ 1 }"), None, origin()))
            .unwrap();
        let b = s
            .put_version(NodeVersion::new(&node("{ 1 }"), None, origin()))
            .unwrap();
        let c = s
            .put_version(NodeVersion::new(&node("{ 2 }"), Some(a.clone()), origin()))
            .unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(s.versions_of(&NodeId("f".into())).unwrap(), vec![a, c]);
    }

    #[test]
    fn version_round_trips_to_node() {
        let n = node("{ 1 }");
        assert_eq!(NodeVersion::new(&n, None, origin()).to_node(), n);
    }

    #[test]
    fn views_round_trip_and_ids_depend_on_content() {
        let mut s = MemoryStore::default();
        let v1 = View::new(
            RunId("r1".into()),
            None,
            BTreeMap::from([(NodeId("f".into()), VersionId("a".into()))]),
            BTreeMap::new(),
            1,
        );
        let v2 = View::new(
            RunId("r1".into()),
            Some(v1.id.clone()),
            BTreeMap::from([(NodeId("f".into()), VersionId("b".into()))]),
            BTreeMap::new(),
            2,
        );
        assert_ne!(v1.id, v2.id);
        s.put_view(v1.clone()).unwrap();
        s.put_view(v2.clone()).unwrap();
        assert_eq!(s.get_view(&v1.id).unwrap(), v1);
        assert_eq!(s.list_views().unwrap(), vec![v1.id, v2.id]);
        assert!(s.get_view(&ViewId("zz".into())).is_err());
    }

    #[test]
    fn hash_is_stable_and_length_prefixed() {
        assert_eq!(content_hash(&["a", "b"]), content_hash(&["a", "b"]));
        assert_ne!(content_hash(&["ab", ""]), content_hash(&["a", "b"]));
    }
}
