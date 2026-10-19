use crate::model::{Graph, NodeId, NodeKind};
use crate::order;

pub const MAX_REVISIONS: usize = 3;

/// Replace a node's signature, reset the bodies of the node and every
/// transitive dependent to unfilled, and return the invalidated ids. Test
/// nodes are not reset.
pub fn apply_signature(graph: &mut Graph, id: &NodeId, new_signature: &str) -> Vec<NodeId> {
    let Some(node) = graph.nodes.get_mut(id) else {
        return Vec::new();
    };
    node.signature = new_signature.to_string();
    node.body = None;
    let mut invalidated = vec![id.clone()];
    for dep in order::dependents(graph, id) {
        if let Some(n) = graph.nodes.get_mut(&dep) {
            if n.kind != NodeKind::Test {
                n.body = None;
                invalidated.push(dep);
            }
        }
    }
    invalidated.sort();
    invalidated.dedup();
    invalidated
}

/// Recognises `REVISE <node id>` on one line followed by the new signature.
pub fn parse_revision_request(response: &str) -> Option<(NodeId, String)> {
    let lines: Vec<&str> = response.lines().collect();
    let start = lines
        .iter()
        .position(|l| l.trim().starts_with("REVISE ") && !l.trim()[7..].trim().is_empty())?;
    let id = lines[start].trim()[7..].trim().to_string();
    let sig: Vec<&str> = lines[start + 1..]
        .iter()
        .copied()
        .skip_while(|l| l.trim().is_empty())
        .take_while(|l| !l.trim_start().starts_with("```"))
        .collect();
    let sig = sig.join("\n");
    let sig = sig.trim();
    if sig.is_empty() {
        let fenced = lines[start + 1..]
            .iter()
            .skip_while(|l| !l.trim_start().starts_with("```"))
            .skip(1)
            .take_while(|l| !l.trim_start().starts_with("```"))
            .copied()
            .collect::<Vec<_>>()
            .join("\n");
        let fenced = fenced.trim();
        if fenced.is_empty() {
            return None;
        }
        return Some((NodeId(id), fenced.to_string()));
    }
    Some((NodeId(id), sig.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Node, Placement};

    fn node(id: &str, kind: NodeKind) -> Node {
        Node {
            id: NodeId(id.into()),
            kind,
            signature: format!("fn {id}()"),
            body: Some("{ }".into()),
            impl_header: None,
            placement: Placement {
                file: "lib.rs".into(),
                index: 0,
            },
        }
    }

    fn nid(s: &str) -> NodeId {
        NodeId(s.into())
    }

    fn graph() -> Graph {
        let mut g = Graph::default();
        for (id, k) in [
            ("a", NodeKind::Function),
            ("b", NodeKind::Function),
            ("c", NodeKind::Function),
            ("d", NodeKind::Function),
            ("t", NodeKind::Test),
        ] {
            g.insert(node(id, k));
        }
        g.add_use(&nid("b"), &nid("a"));
        g.add_use(&nid("c"), &nid("b"));
        g.add_use(&nid("t"), &nid("a"));
        g
    }

    #[test]
    fn resets_dependents_and_spares_tests() {
        let mut g = graph();
        let out = apply_signature(&mut g, &nid("a"), "fn a(x: u32)");
        assert_eq!(out, vec![nid("a"), nid("b"), nid("c")]);
        assert_eq!(g.nodes[&nid("a")].signature, "fn a(x: u32)");
        for id in ["a", "b", "c"] {
            assert!(g.nodes[&nid(id)].body.is_none());
        }
        assert!(g.nodes[&nid("d")].body.is_some());
        assert!(g.nodes[&nid("t")].body.is_some());
    }

    #[test]
    fn unknown_id_is_noop() {
        let mut g = graph();
        let before = g.clone();
        assert!(apply_signature(&mut g, &nid("zzz"), "fn z()").is_empty());
        assert_eq!(g, before);
    }

    #[test]
    fn parses_plain_request() {
        let r = parse_revision_request("REVISE a\npub fn a(x: u32) -> u32").unwrap();
        assert_eq!(r, (nid("a"), "pub fn a(x: u32) -> u32".to_string()));
    }

    #[test]
    fn parses_fenced_request_with_preamble() {
        let r = parse_revision_request(
            "Need change.\nREVISE Foo::new\n```rust\npub fn new(\n    x: u32,\n) -> Foo\n```\n",
        )
        .unwrap();
        assert_eq!(r.0, nid("Foo::new"));
        assert_eq!(r.1, "pub fn new(\n    x: u32,\n) -> Foo");
    }

    #[test]
    fn rejects_non_requests() {
        assert!(parse_revision_request("{ Ok(()) }").is_none());
        assert!(parse_revision_request("REVISE a").is_none());
        assert!(parse_revision_request("REVISE\nfn a()").is_none());
    }
}
