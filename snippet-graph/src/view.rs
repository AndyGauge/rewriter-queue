use crate::model::{Graph, NodeId};
use crate::version::{NodeVersion, Origin, RunId, Store, StoreError, VersionId, View, ViewId};
use std::collections::BTreeMap;

/// Store a version for every node in `graph` (reusing identical content) and
/// return the view over them. `parent` is the view this one derives from; each
/// node's new version records its previous version in that view as its parent.
pub fn from_graph(
    store: &mut dyn Store,
    graph: &Graph,
    run: &RunId,
    origin: &Origin,
    parent: Option<&View>,
) -> Result<View, StoreError> {
    let mut versions = BTreeMap::new();
    for (id, node) in &graph.nodes {
        let prior = parent.and_then(|p| p.versions.get(id)).cloned();
        let mut version = NodeVersion::new(node, None, origin.clone());
        if prior.as_ref() != Some(&version.id) {
            version.parent = prior;
        }
        versions.insert(id.clone(), store.put_version(version)?);
    }
    Ok(View::new(
        run.clone(),
        parent.map(|p| p.id.clone()),
        versions,
        graph.uses.clone(),
        origin.created_ms,
    ))
}

/// Render a view back to a graph, with nodes from stored versions and edges from
/// the view. `to_graph(from_graph(g)) == g`.
pub fn to_graph(store: &dyn Store, view: &View) -> Result<Graph, StoreError> {
    let mut graph = Graph {
        uses: view.uses.clone(),
        ..Graph::default()
    };
    for (id, version) in &view.versions {
        graph
            .nodes
            .insert(id.clone(), store.get_version(version)?.to_node());
    }
    Ok(graph)
}

/// Nodes whose version differs between the views, including nodes present in
/// only one. Sorted.
pub fn diff(a: &View, b: &View) -> Vec<NodeId> {
    let mut out: Vec<NodeId> = a
        .versions
        .iter()
        .filter(|(id, v)| b.versions.get(*id) != Some(*v))
        .map(|(id, _)| id.clone())
        .chain(
            b.versions
                .keys()
                .filter(|id| !a.versions.contains_key(*id))
                .cloned(),
        )
        .collect();
    out.sort();
    out
}

/// A new view equal to `view` except `node` uses `version`; its parent is
/// `view`. Err when the node is not in the view.
pub fn swap(
    view: &View,
    node: &NodeId,
    version: VersionId,
    created_ms: u64,
) -> Result<View, StoreError> {
    if !view.versions.contains_key(node) {
        return Err(StoreError::NotFound(node.0.clone()));
    }
    let mut versions = view.versions.clone();
    versions.insert(node.clone(), version);
    Ok(View::new(
        view.run.clone(),
        Some(view.id.clone()),
        versions,
        view.uses.clone(),
        created_ms,
    ))
}

/// A new view equal to `view` except each listed node uses its listed version;
/// its parent is `view`. Err when a node is not in the view.
pub fn swap_many(
    view: &View,
    swaps: &[(NodeId, VersionId)],
    created_ms: u64,
) -> Result<View, StoreError> {
    let mut versions = view.versions.clone();
    for (node, version) in swaps {
        if !versions.contains_key(node) {
            return Err(StoreError::NotFound(node.0.clone()));
        }
        versions.insert(node.clone(), version.clone());
    }
    Ok(View::new(
        view.run.clone(),
        Some(view.id.clone()),
        versions,
        view.uses.clone(),
        created_ms,
    ))
}

/// The view `view` derives from, if any.
pub fn undo(store: &dyn Store, view: &View) -> Result<Option<View>, StoreError> {
    view.parent.as_ref().map(|p| store.get_view(p)).transpose()
}

/// The chain of views from `view` back to the root, newest first.
pub fn lineage(store: &dyn Store, view: &ViewId) -> Result<Vec<View>, StoreError> {
    let mut out = Vec::new();
    let mut next = Some(view.clone());
    while let Some(id) = next {
        let v = store.get_view(&id)?;
        next = v.parent.clone();
        out.push(v);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Node, NodeKind, Placement};
    use crate::version::MemoryStore;

    fn origin() -> Origin {
        Origin {
            run: RunId("r".into()),
            model: "m".into(),
            prompt_hash: "p".into(),
            created_ms: 7,
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

    fn graph(bodies: &[(&str, &str)]) -> Graph {
        let mut g = Graph::default();
        for (n, b) in bodies {
            g.insert(node(n, b));
        }
        g.add_use(&NodeId("a".into()), &NodeId("b".into()));
        g
    }

    fn build(store: &mut MemoryStore, g: &Graph, parent: Option<&View>) -> View {
        let v = from_graph(store, g, &RunId("r".into()), &origin(), parent).unwrap();
        store.put_view(v.clone()).unwrap();
        v
    }

    #[test]
    fn round_trip_equals_original() {
        let mut s = MemoryStore::default();
        let g = graph(&[("a", "{ 1 }"), ("b", "{ 2 }")]);
        let v = build(&mut s, &g, None);
        assert_eq!(to_graph(&s, &v).unwrap(), g);
    }

    #[test]
    fn diff_names_changed_nodes() {
        let mut s = MemoryStore::default();
        let v1 = build(
            &mut s,
            &graph(&[("a", "{ 1 }"), ("b", "{ 2 }"), ("c", "{ 3 }")]),
            None,
        );
        let v2 = build(
            &mut s,
            &graph(&[("a", "{ 1 }"), ("b", "{ 9 }"), ("d", "{ 4 }")]),
            Some(&v1),
        );
        let names: Vec<String> = diff(&v1, &v2).into_iter().map(|n| n.0).collect();
        assert_eq!(names, ["b", "c", "d"]);
        assert!(diff(&v1, &v1).is_empty());
    }

    #[test]
    fn changed_node_records_parent_version() {
        let mut s = MemoryStore::default();
        let v1 = build(&mut s, &graph(&[("a", "{ 1 }"), ("b", "{ 2 }")]), None);
        let v2 = build(&mut s, &graph(&[("a", "{ 1 }"), ("b", "{ 9 }")]), Some(&v1));
        let b = NodeId("b".into());
        let new = s.get_version(&v2.versions[&b]).unwrap();
        assert_eq!(new.parent.as_ref(), Some(&v1.versions[&b]));
        let same = s.get_version(&v2.versions[&NodeId("a".into())]).unwrap();
        assert_eq!(same.parent, None);
    }

    #[test]
    fn swap_changes_one_entry() {
        let mut s = MemoryStore::default();
        let v1 = build(&mut s, &graph(&[("a", "{ 1 }"), ("b", "{ 2 }")]), None);
        let other = s
            .put_version(NodeVersion::new(&node("b", "{ 5 }"), None, origin()))
            .unwrap();
        let b = NodeId("b".into());
        let v2 = swap(&v1, &b, other.clone(), 9).unwrap();
        assert_eq!(diff(&v1, &v2), vec![b.clone()]);
        assert_eq!(v2.versions[&b], other);
        assert_eq!(v2.parent.as_ref(), Some(&v1.id));
        assert_eq!(v2.uses, v1.uses);
        let missing = swap(&v1, &NodeId("zz".into()), other, 9);
        assert_eq!(missing, Err(StoreError::NotFound("zz".into())));
    }

    #[test]
    fn undo_and_lineage_follow_parents() {
        let mut s = MemoryStore::default();
        let v1 = build(&mut s, &graph(&[("a", "{ 1 }"), ("b", "{ 2 }")]), None);
        let alt = s
            .put_version(NodeVersion::new(&node("b", "{ 5 }"), None, origin()))
            .unwrap();
        let v2 = swap(&v1, &NodeId("b".into()), alt, 9).unwrap();
        s.put_view(v2.clone()).unwrap();
        assert_eq!(undo(&s, &v2).unwrap(), Some(v1.clone()));
        assert_eq!(undo(&s, &v1).unwrap(), None);
        assert_eq!(lineage(&s, &v2.id).unwrap(), vec![v2, v1]);
    }

    #[test]
    fn identical_graph_twice_gives_same_view() {
        let mut s = MemoryStore::default();
        let g = graph(&[("a", "{ 1 }"), ("b", "{ 2 }")]);
        let v1 = build(&mut s, &g, None);
        let v2 = build(&mut s, &g, None);
        assert_eq!(v1, v2);
        assert_eq!(s.list_views().unwrap().len(), 1);
    }
}
