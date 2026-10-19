use crate::model::{Graph, Node, NodeId, NodeKind};
use std::collections::BTreeSet;
use syn::visit::Visit;

/// Add method-call edges to `graph.uses` on top of the identifier-name edges
/// already present: `Type::method(..)` links to the `Type::method` node, and
/// `x.method(..)` links to every Method node named `method` (all candidates
/// when ambiguous). Test nodes get edges to what their bodies call. Must be
/// idempotent and must never remove existing edges or add self-edges.
pub fn resolve(graph: &mut Graph) {
    let methods: Vec<&NodeId> = graph
        .nodes
        .values()
        .filter(|n| n.kind == NodeKind::Method)
        .map(|n| &n.id)
        .collect();

    let mut found: Vec<(NodeId, NodeId)> = Vec::new();
    for node in graph.nodes.values() {
        let Ok(item) = syn::parse_str::<syn::Item>(&node.render()) else {
            continue;
        };
        let mut collector = Collector {
            self_type: self_type(node),
            paths: BTreeSet::new(),
            receivers: BTreeSet::new(),
        };
        collector.visit_item(&item);

        for path in &collector.paths {
            let id = NodeId(path.clone());
            if graph
                .nodes
                .get(&id)
                .is_some_and(|n| n.kind == NodeKind::Method)
            {
                found.push((node.id.clone(), id));
            }
        }
        for name in &collector.receivers {
            let suffix = format!("::{name}");
            for id in methods.iter().filter(|id| id.0.ends_with(&suffix)) {
                found.push((node.id.clone(), (*id).clone()));
            }
        }
    }

    for (from, to) in found {
        if from != to {
            graph.add_use(&from, &to);
        }
    }
}

fn self_type(node: &Node) -> Option<String> {
    let header = node.impl_header.as_deref()?;
    let item: syn::ItemImpl = syn::parse_str(&format!("{header} {{}}")).ok()?;
    match *item.self_ty {
        syn::Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    }
}

struct Collector {
    self_type: Option<String>,
    paths: BTreeSet<String>,
    receivers: BTreeSet<String>,
}

impl<'ast> Visit<'ast> for Collector {
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*call.func {
            let segs = &p.path.segments;
            if segs.len() >= 2 {
                let ty = segs[segs.len() - 2].ident.to_string();
                let method = segs[segs.len() - 1].ident.to_string();
                let ty = if ty == "Self" {
                    self.self_type.clone()
                } else {
                    Some(ty)
                };
                if let Some(ty) = ty {
                    self.paths.insert(format!("{ty}::{method}"));
                }
            }
        }
        syn::visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        self.receivers.insert(call.method.to_string());
        syn::visit::visit_expr_method_call(self, call);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Placement;

    fn node(id: &str, kind: NodeKind, sig: &str, body: &str, header: Option<&str>) -> Node {
        Node {
            id: NodeId(id.into()),
            kind,
            signature: sig.into(),
            body: Some(body.into()),
            impl_header: header.map(String::from),
            placement: Placement {
                file: "lib.rs".into(),
                index: 0,
            },
        }
    }

    fn graph(nodes: Vec<Node>) -> Graph {
        let mut g = Graph::default();
        for n in nodes {
            g.insert(n);
        }
        g
    }

    fn id(s: &str) -> NodeId {
        NodeId(s.into())
    }

    #[test]
    fn path_call_links_method() {
        let mut g = graph(vec![
            node(
                "Foo::new",
                NodeKind::Method,
                "pub fn new() -> Foo",
                "{ Foo }",
                Some("impl Foo"),
            ),
            node(
                "run",
                NodeKind::Function,
                "fn run()",
                "{ let _ = Foo::new(); }",
                None,
            ),
        ]);
        resolve(&mut g);
        assert!(g.dependencies(&id("run")).any(|d| d == &id("Foo::new")));
    }

    #[test]
    fn unique_receiver_call_links() {
        let mut g = graph(vec![
            node(
                "Foo::go",
                NodeKind::Method,
                "fn go(&self)",
                "{}",
                Some("impl Foo"),
            ),
            node(
                "run",
                NodeKind::Function,
                "fn run(x: Foo)",
                "{ x.go(); }",
                None,
            ),
        ]);
        resolve(&mut g);
        let deps: Vec<_> = g.dependencies(&id("run")).collect();
        assert_eq!(deps, vec![&id("Foo::go")]);
    }

    #[test]
    fn ambiguous_receiver_call_links_all() {
        let mut g = graph(vec![
            node(
                "Foo::go",
                NodeKind::Method,
                "fn go(&self)",
                "{}",
                Some("impl Foo"),
            ),
            node(
                "Bar::go",
                NodeKind::Method,
                "fn go(&self)",
                "{}",
                Some("impl Bar"),
            ),
            node(
                "t",
                NodeKind::Test,
                "#[test] fn t()",
                "{ make().go(); }",
                None,
            ),
        ]);
        resolve(&mut g);
        let deps: Vec<_> = g.dependencies(&id("t")).collect();
        assert_eq!(deps, vec![&id("Bar::go"), &id("Foo::go")]);
    }

    #[test]
    fn self_path_resolves_via_impl_header() {
        let mut g = graph(vec![
            node(
                "Foo::new",
                NodeKind::Method,
                "fn new() -> Foo",
                "{ Foo }",
                Some("impl Foo"),
            ),
            node(
                "Foo::dup",
                NodeKind::Method,
                "fn dup() -> Foo",
                "{ Self::new() }",
                Some("impl Foo"),
            ),
        ]);
        resolve(&mut g);
        assert!(g
            .dependencies(&id("Foo::dup"))
            .any(|d| d == &id("Foo::new")));
    }

    #[test]
    fn idempotent_no_self_edges_keeps_existing() {
        let mut g = graph(vec![
            node(
                "Foo::rec",
                NodeKind::Method,
                "fn rec(&self)",
                "{ self.rec(); Foo::rec(self); }",
                Some("impl Foo"),
            ),
            node("other", NodeKind::Function, "fn other()", "{}", None),
        ]);
        g.add_use(&id("Foo::rec"), &id("other"));
        resolve(&mut g);
        let once = g.clone();
        resolve(&mut g);
        assert_eq!(g, once);
        let deps: Vec<_> = g.dependencies(&id("Foo::rec")).collect();
        assert_eq!(deps, vec![&id("other")]);
    }

    #[test]
    fn unparsable_text_is_ignored() {
        let mut g = graph(vec![
            node(
                "Foo::go",
                NodeKind::Method,
                "fn go(&self)",
                "{}",
                Some("impl Foo"),
            ),
            node("bad", NodeKind::Function, "fn bad(", "{ x.go( }", None),
        ]);
        resolve(&mut g);
        assert!(g.uses.is_empty());
    }
}
