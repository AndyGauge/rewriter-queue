use crate::model::{Graph, NodeId, NodeKind};
use syn::visit::Visit;

const MAX_CODE_LINES: usize = 30;
const MAX_COMPLEXITY: usize = 8;
const MAX_DEPTH: usize = 3;
const MAX_TESTS: usize = 5;
/// A body this small is never too large, whatever the model says.
const TRIVIAL_LINES: usize = 8;
const TRIVIAL_COMPLEXITY: usize = 3;

/// What the fill prompt asks for when a function needs several separate steps.
pub const MARKER_PREFIX: &str = "// TODO: does too much";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Measure {
    pub code_lines: usize,
    /// One plus the number of branches, match arms beyond the first, loops and
    /// short-circuit operators.
    pub complexity: usize,
    pub depth: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The model wrote the marker comment at the top of the body.
    Marker,
    /// The measured body is over a threshold.
    Measured,
    /// So many tests reach the function that it is probably several functions.
    TestLoad,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub node: NodeId,
    pub kind: Kind,
    pub detail: String,
}

#[derive(Default)]
struct Counter {
    branches: usize,
    depth: usize,
    max_depth: usize,
}

impl<'ast> Visit<'ast> for Counter {
    fn visit_block(&mut self, block: &'ast syn::Block) {
        self.depth += 1;
        self.max_depth = self.max_depth.max(self.depth);
        syn::visit::visit_block(self, block);
        self.depth -= 1;
    }

    fn visit_expr_if(&mut self, e: &'ast syn::ExprIf) {
        self.branches += 1;
        syn::visit::visit_expr_if(self, e);
    }

    fn visit_expr_match(&mut self, e: &'ast syn::ExprMatch) {
        self.branches += e.arms.len().saturating_sub(1);
        syn::visit::visit_expr_match(self, e);
    }

    fn visit_expr_for_loop(&mut self, e: &'ast syn::ExprForLoop) {
        self.branches += 1;
        syn::visit::visit_expr_for_loop(self, e);
    }

    fn visit_expr_while(&mut self, e: &'ast syn::ExprWhile) {
        self.branches += 1;
        syn::visit::visit_expr_while(self, e);
    }

    fn visit_expr_loop(&mut self, e: &'ast syn::ExprLoop) {
        self.branches += 1;
        syn::visit::visit_expr_loop(self, e);
    }

    fn visit_expr_binary(&mut self, e: &'ast syn::ExprBinary) {
        if matches!(e.op, syn::BinOp::And(_) | syn::BinOp::Or(_)) {
            self.branches += 1;
        }
        syn::visit::visit_expr_binary(self, e);
    }
}

fn code_lines(body: &str) -> usize {
    body.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("//") && *l != "{" && *l != "}")
        .count()
}

/// Size, branching and nesting of a function body written as `{ ... }`.
/// `None` if the body does not parse.
pub fn measure(body: &str) -> Option<Measure> {
    let block: syn::Block = syn::parse_str(body).ok()?;
    let mut counter = Counter::default();
    counter.visit_block(&block);
    Some(Measure {
        code_lines: code_lines(body),
        complexity: 1 + counter.branches,
        depth: counter.max_depth.saturating_sub(1),
    })
}

/// The reason after a leading `// TODO: does too much` comment, if the body
/// starts with one.
pub fn marker(body: &str) -> Option<String> {
    let first = body
        .trim_start()
        .strip_prefix('{')?
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())?;
    let lower = first.to_ascii_lowercase();
    if !lower.starts_with("//") || !lower.contains("todo") || !lower.contains("too much") {
        return None;
    }
    let after = &first[lower.find("too much")? + "too much".len()..];
    Some(
        after
            .trim_matches(|c: char| c == '-' || c == ':' || c == '.' || c.is_whitespace())
            .to_string(),
    )
}

fn over_limits(m: &Measure) -> Vec<String> {
    let mut out = Vec::new();
    if m.code_lines > MAX_CODE_LINES {
        out.push(format!("{} lines of code (limit {MAX_CODE_LINES})", m.code_lines));
    }
    if m.complexity > MAX_COMPLEXITY {
        out.push(format!("complexity {} (limit {MAX_COMPLEXITY})", m.complexity));
    }
    if m.depth > MAX_DEPTH {
        out.push(format!("nesting {} deep (limit {MAX_DEPTH})", m.depth));
    }
    out
}

/// Structural findings for one body: the model's own marker and any measured
/// limit it is over.
pub fn evaluate_body(node: &NodeId, body: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    let measured = measure(body);
    let trivial = measured
        .as_ref()
        .is_some_and(|m| m.code_lines <= TRIVIAL_LINES && m.complexity <= TRIVIAL_COMPLEXITY);
    if let Some(reason) = marker(body).filter(|_| !trivial) {
        out.push(Finding {
            node: node.clone(),
            kind: Kind::Marker,
            detail: if reason.is_empty() {
                "marked by the model as doing too much".to_string()
            } else {
                reason
            },
        });
    }
    if let Some(m) = measured {
        let limits = over_limits(&m);
        if !limits.is_empty() {
            out.push(Finding {
                node: node.clone(),
                kind: Kind::Measured,
                detail: limits.join(", "),
            });
        }
    }
    out
}

/// Tests written for the function itself: those that use it and carry its name.
/// Tests of its callers also reach it, but they say nothing about its size.
fn own_tests(graph: &Graph, node: &NodeId) -> usize {
    let leaf = node.0.rsplit("::").next().unwrap_or(&node.0);
    graph
        .tests_for(node)
        .into_iter()
        .filter(|t| t.0.contains(leaf))
        .count()
}

/// A finding when more than `MAX_TESTS` tests were written for the function.
pub fn test_load(graph: &Graph, node: &NodeId) -> Option<Finding> {
    let n = own_tests(graph, node);
    (n > MAX_TESTS).then(|| Finding {
        node: node.clone(),
        kind: Kind::TestLoad,
        detail: format!("{n} tests written for it (limit {MAX_TESTS})"),
    })
}

/// Findings for every function and method in the graph. A function with no
/// body is judged by `last_body`, its most recent attempt.
pub fn evaluate(graph: &Graph, last_body: &dyn Fn(&NodeId) -> Option<String>) -> Vec<Finding> {
    let mut out = Vec::new();
    for n in graph
        .nodes
        .values()
        .filter(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method))
    {
        let body = n.body.clone().or_else(|| last_body(&n.id));
        if let Some(body) = body {
            out.extend(evaluate_body(&n.id, &body));
        }
        out.extend(test_load(graph, &n.id));
    }
    out
}

/// The sentence a retry prompt carries when an earlier attempt was too large.
pub fn refactor_reason(findings: &[Finding]) -> Option<String> {
    (!findings.is_empty()).then(|| {
        findings
            .iter()
            .map(|f| f.detail.clone())
            .collect::<Vec<_>>()
            .join("; ")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> NodeId {
        NodeId("f".into())
    }

    #[test]
    fn small_body_is_clean() {
        let findings = evaluate_body(&id(), "{ let x = 1;\n x + 2 }");
        assert!(findings.is_empty());
        let m = measure("{ let x = 1;\n x + 2 }").unwrap();
        assert_eq!((m.complexity, m.depth), (1, 0));
    }

    #[test]
    fn marker_reason_is_parsed() {
        let lines: String = (0..9).map(|i| format!("    let v{i} = {i};\n")).collect();
        let body = format!("{{\n    // TODO: does too much: filters, sorts and sums\n{lines}    v0\n}}");
        assert_eq!(marker(&body).as_deref(), Some("filters, sorts and sums"));
        let findings = evaluate_body(&id(), &body);
        assert_eq!(findings[0].kind, Kind::Marker);
        assert_eq!(findings[0].detail, "filters, sorts and sums");
    }

    #[test]
    fn marker_on_a_trivial_body_is_ignored() {
        let body = "{\n    // TODO: does too much: sums\n    order.lines.iter().map(line_total).sum()\n}";
        assert!(marker(body).is_some());
        assert!(evaluate_body(&id(), body).is_empty());
    }

    #[test]
    fn marker_without_reason_and_other_comments() {
        assert_eq!(marker("{ // TODO this method does too much\n x }").as_deref(), Some(""));
        assert_eq!(marker("{ // TODO: later\n x }"), None);
        assert_eq!(marker("{ // does too much\n x }"), None);
        assert_eq!(marker("{ let x = 1; // TODO: does too much\n x }"), None);
    }

    #[test]
    fn complexity_counts_branches_loops_and_arms() {
        let body = "{
            let mut n = 0;
            for i in 0..3 {
                if i > 0 && i < 2 {
                    n += match i { 0 => 1, 1 => 2, _ => 3 };
                }
            }
            n
        }";
        let m = measure(body).unwrap();
        assert_eq!(m.complexity, 1 + 1 + 1 + 1 + 2);
        assert_eq!(m.depth, 2);
    }

    #[test]
    fn deep_nesting_is_flagged_with_the_limit() {
        let body = "{ if a { if b { if c { if d { if e { f() } } } } } }";
        let findings = evaluate_body(&id(), body);
        let measured = findings.iter().find(|f| f.kind == Kind::Measured).unwrap();
        assert!(measured.detail.contains("nesting 5 deep (limit 3)"));
    }

    #[test]
    fn long_body_is_flagged_and_comments_do_not_count() {
        let lines: String = (0..31).map(|i| format!("    let v{i} = {i};\n")).collect();
        let long = format!("{{\n{lines}    v0\n}}");
        let findings = evaluate_body(&id(), &long);
        assert!(findings[0].detail.contains("lines of code (limit 30)"));
        let comments: String = (0..40).map(|i| format!("    // note {i}\n")).collect();
        let commented = format!("{{\n{comments}    1\n}}");
        assert!(evaluate_body(&id(), &commented).is_empty());
    }

    #[test]
    fn many_tests_are_a_finding() {
        use crate::model::{Node, Placement};
        let mut g = Graph::default();
        let mk = |name: &str, kind| Node {
            id: NodeId(name.into()),
            kind,
            signature: format!("fn {name}()"),
            body: Some("{}".into()),
            impl_header: None,
            placement: Placement {
                file: "src/lib.rs".into(),
                index: 0,
            },
        };
        g.nodes.insert(NodeId("f".into()), mk("f", NodeKind::Function));
        for i in 0..6 {
            let name = format!("f_case{i}");
            g.nodes.insert(NodeId(name.clone()), mk(&name, NodeKind::Test));
            g.uses
                .entry(NodeId(name))
                .or_default()
                .insert(NodeId("f".into()));
        }
        for i in 0..6 {
            let name = format!("caller_case{i}");
            g.nodes.insert(NodeId(name.clone()), mk(&name, NodeKind::Test));
            g.uses
                .entry(NodeId(name))
                .or_default()
                .insert(NodeId("f".into()));
        }
        let found = evaluate(&g, &|_| None);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].kind, Kind::TestLoad);
        assert_eq!(found[0].detail, "6 tests written for it (limit 5)");
    }

    #[test]
    fn unparseable_body_is_not_measured() {
        assert!(measure("{ let = ; ").is_none());
        assert!(evaluate_body(&id(), "{ let = ; ").is_empty());
    }
}
