use crate::model::{Graph, NodeId, NodeKind};
use std::collections::BTreeMap;

/// Remove nodes the linter flags as dead weight: `use` items reported as
/// unused imports, and private functions or methods reported as never used,
/// together with the tests that reach them. Frozen nodes are never removed.
/// Returns the removed ids.
pub fn prune(graph: &mut Graph, by_node: &BTreeMap<NodeId, Vec<String>>) -> Vec<NodeId> {
    let mut removed = Vec::new();
    for (id, messages) in by_node {
        let Some(node) = graph.nodes.get(id) else {
            continue;
        };
        if graph.frozen.contains(id) {
            continue;
        }
        let dead = match node.kind {
            NodeKind::Use => messages.iter().any(|m| m.contains("unused import")),
            NodeKind::Function | NodeKind::Method => {
                !node.signature.trim_start().starts_with("pub")
                    && messages.iter().any(|m| m.contains("is never used"))
            }
            _ => false,
        };
        if !dead {
            continue;
        }
        let tests: Vec<NodeId> = graph.tests_for(id).into_iter().cloned().collect();
        for gone in tests.iter().chain(std::iter::once(id)) {
            graph.nodes.remove(gone);
            graph.uses.remove(gone);
            removed.push(gone.clone());
        }
    }
    for deps in graph.uses.values_mut() {
        deps.retain(|d| graph.nodes.contains_key(d));
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Node, Placement};

    fn node(id: &str, kind: NodeKind, sig: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            kind,
            signature: sig.into(),
            body: Some("{}".into()),
            impl_header: None,
            placement: Placement {
                file: "lib.rs".into(),
                index: 0,
            },
        }
    }

    fn id(s: &str) -> NodeId {
        NodeId(s.into())
    }

    fn graph() -> Graph {
        let mut g = Graph::default();
        g.insert(node("use a::B;", NodeKind::Use, "use a::B;"));
        g.insert(node("helper", NodeKind::Function, "fn helper()"));
        g.insert(node("api", NodeKind::Function, "pub fn api()"));
        g.insert(node("test::helper::t", NodeKind::Test, "#[test] fn t()"));
        g.add_use(&id("test::helper::t"), &id("helper"));
        g
    }

    #[test]
    fn removes_unused_imports_and_private_dead_functions_with_their_tests() {
        let mut g = graph();
        let by_node = BTreeMap::from([
            (
                id("use a::B;"),
                vec!["warning: unused import: `a::B`".into()],
            ),
            (
                id("helper"),
                vec!["warning: function `helper` is never used".into()],
            ),
        ]);
        let removed = prune(&mut g, &by_node);
        assert_eq!(removed.len(), 3);
        assert!(g.nodes.contains_key(&id("api")));
        assert!(!g.nodes.contains_key(&id("helper")));
        assert!(!g.nodes.contains_key(&id("test::helper::t")));
        assert!(g.uses.is_empty());
    }

    #[test]
    fn keeps_public_frozen_and_unrelated_warnings() {
        let mut g = graph();
        g.frozen.insert(id("helper"));
        let by_node = BTreeMap::from([
            (
                id("api"),
                vec!["warning: function `api` is never used".into()],
            ),
            (
                id("helper"),
                vec!["warning: function `helper` is never used".into()],
            ),
            (id("use a::B;"), vec!["warning: something else".into()]),
        ]);
        assert!(prune(&mut g, &by_node).is_empty());
        assert_eq!(g.nodes.len(), 4);
    }
}
