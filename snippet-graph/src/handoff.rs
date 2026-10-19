use crate::blame;
use crate::export::{commit_all, init_repo, write_files};
use crate::model::{Graph, NodeId, NodeKind, TestReport};
use crate::order;
use crate::structure;
use crate::version::{Store, ViewId};
use crate::view::{diff, lineage, to_graph};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Passing,
    FailingTests,
    DoesNotCompile,
    Unfilled,
    Unverified,
}

impl Status {
    fn label(self) -> &'static str {
        match self {
            Status::Passing => "passing",
            Status::FailingTests => "compiles, tests fail",
            Status::DoesNotCompile => "does not compile",
            Status::Unfilled => "unfilled (todo!)",
            Status::Unverified => "unverified (crate does not build)",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeReport {
    pub node: NodeId,
    pub status: Status,
    /// 0 for a node that uses no other function; one more than its deepest dependency otherwise.
    pub depth: usize,
    pub attempts: usize,
    /// True when the node used its whole attempt budget.
    pub budget_spent: bool,
    pub last_error: Option<String>,
    /// Why the function looks like it does too much, if it does.
    pub structure: Vec<String>,
}

fn fillable(graph: &Graph, id: &NodeId) -> bool {
    graph
        .nodes
        .get(id)
        .is_some_and(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method))
}

fn first_line(text: &str) -> String {
    let line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or_default();
    if line.chars().count() > 160 {
        let cut: String = line.chars().take(157).collect();
        format!("{cut}...")
    } else {
        line.to_string()
    }
}

/// One report per function and method, with its status judged from the final
/// test report. `failures` holds the last error for nodes the run gave up on.
pub fn node_reports(
    graph: &Graph,
    report: &TestReport,
    failures: &BTreeMap<NodeId, String>,
    store: &dyn Store,
) -> Vec<NodeReport> {
    let depths: BTreeMap<NodeId, usize> = order::levels(graph)
        .map(|levels| {
            levels
                .into_iter()
                .enumerate()
                .flat_map(|(i, ids)| ids.into_iter().map(move |id| (id, i)))
                .collect()
        })
        .unwrap_or_default();
    let mut structure: BTreeMap<NodeId, Vec<String>> = BTreeMap::new();
    let last_body = |id: &NodeId| {
        let latest = store.versions_of(id).ok()?.pop()?;
        store.get_version(&latest).ok()?.body
    };
    for f in structure::evaluate(graph, &last_body) {
        structure.entry(f.node).or_default().push(f.detail);
    }
    let suspects: BTreeSet<NodeId> = blame::rank(graph, &report.outcomes, &[])
        .into_iter()
        .map(|s| s.node)
        .collect();

    graph
        .nodes
        .keys()
        .filter(|id| fillable(graph, id))
        .map(|id| {
            let node = &graph.nodes[id];
            let build_error = report
                .build_errors
                .get(id)
                .and_then(|errs| errs.first())
                .map(|e| first_line(e));
            let test_error = || {
                report
                    .outcomes
                    .iter()
                    .filter(|o| !o.passed && graph.subjects_of(&o.test).contains(&id))
                    .map(|o| first_line(&o.message))
                    .next()
            };
            let (status, last_error) = if !node.is_filled() {
                (Status::Unfilled, failures.get(id).map(|e| first_line(e)))
            } else if build_error.is_some() {
                (Status::DoesNotCompile, build_error)
            } else if !report.compiled {
                (Status::Unverified, None)
            } else if suspects.contains(id) {
                (Status::FailingTests, test_error())
            } else {
                (Status::Passing, None)
            };
            NodeReport {
                node: id.clone(),
                status,
                depth: depths.get(id).copied().unwrap_or(0),
                attempts: store.versions_of(id).map_or(0, |v| v.len()),
                budget_spent: graph.exhausted(id),
                last_error,
                structure: structure.remove(id).unwrap_or_default(),
            }
        })
        .collect()
}

/// Non-passing nodes whose function dependencies all pass: where a person
/// should start.
pub fn start_here(graph: &Graph, reports: &[NodeReport]) -> Vec<NodeId> {
    let status: BTreeMap<&NodeId, Status> = reports.iter().map(|r| (&r.node, r.status)).collect();
    let mut out: Vec<&NodeReport> = reports
        .iter()
        .filter(|r| r.status != Status::Passing)
        .filter(|r| {
            graph
                .dependencies(&r.node)
                .filter_map(|d| status.get(d))
                .all(|s| *s == Status::Passing)
        })
        .collect();
    out.sort_by_key(|r| (r.depth, r.node.clone()));
    out.into_iter().map(|r| r.node.clone()).collect()
}

pub fn render(graph: &Graph, reports: &[NodeReport]) -> String {
    let total = reports.len();
    let passing = reports
        .iter()
        .filter(|r| r.status == Status::Passing)
        .count();
    let mut out = String::from("# Handoff\n\n");
    out.push_str(&format!(
        "{passing} of {total} functions pass their tests. The git history of this repository is the sequence of attempts; each commit message lists the functions that changed and which attempt number they are.\n\n"
    ));
    let start = start_here(graph, reports);
    if start.is_empty() {
        out.push_str("Nothing is blocked.\n\n");
    } else {
        out.push_str("## Start here\n\nNot passing, with every function they use already passing:\n\n");
        for id in &start {
            out.push_str(&format!("- `{}`\n", id.0));
        }
        out.push('\n');
    }
    let flagged: Vec<&NodeReport> = reports.iter().filter(|r| !r.structure.is_empty()).collect();
    if !flagged.is_empty() {
        out.push_str("## Structure\n\nThese look like they do too much. Splitting them is likely to help more than another attempt:\n\n");
        for r in flagged {
            out.push_str(&format!("- `{}`: {}\n", r.node.0, r.structure.join("; ")));
        }
        out.push('\n');
    }
    out.push_str("## All functions\n\nLeaves first (depth 0 uses no other function).\n\n| Function | Status | Depth | Attempts | Last error |\n|---|---|---|---|---|\n");
    let mut sorted: Vec<&NodeReport> = reports.iter().collect();
    sorted.sort_by_key(|r| (r.depth, r.node.clone()));
    for r in sorted {
        out.push_str(&format!(
            "| `{}` | {} | {} | {} | {} |\n",
            r.node.0,
            r.status.label(),
            r.depth,
            if r.budget_spent {
                format!("{} (budget spent)", r.attempts)
            } else {
                r.attempts.to_string()
            },
            r.last_error.as_deref().unwrap_or("").replace('|', "\\|")
        ));
    }
    out
}

fn commit_message(
    store: &dyn Store,
    view: &crate::version::View,
    previous: Option<&crate::version::View>,
) -> String {
    let short: String = view.id.0.chars().take(8).collect();
    let tests = store
        .results_for(&view.id)
        .ok()
        .flatten()
        .map(|r| {
            let failing = r.tests.iter().filter(|t| !t.passed).count();
            format!(" ({failing} of {} tests failing)", r.tests.len())
        })
        .unwrap_or_default();
    let Some(previous) = previous else {
        return format!("view {short}: skeleton, {} nodes{tests}", view.versions.len());
    };
    let changed = diff(previous, view);
    let mut msg = format!("view {short}: {} nodes changed{tests}\n\n", changed.len());
    for id in changed {
        let attempt = view
            .versions
            .get(&id)
            .and_then(|v| {
                store
                    .versions_of(&id)
                    .ok()
                    .and_then(|all| all.iter().position(|x| x == v))
            })
            .map_or(0, |i| i + 1);
        msg.push_str(&format!("- {} (attempt {attempt})\n", id.0));
    }
    msg
}

/// Write `dir` as a git repository with one commit per view from the first to
/// `last`, then a final commit adding `HANDOFF.md`.
pub fn write_repo(
    store: &dyn Store,
    last: &ViewId,
    manifest: Option<&Path>,
    dir: &Path,
    handoff: &str,
) -> Result<(), String> {
    init_repo(dir)?;
    if let Some(manifest) = manifest.filter(|m| m.exists()) {
        std::fs::copy(manifest, dir.join("Cargo.toml")).map_err(|e| e.to_string())?;
    }
    let mut views = lineage(store, last).map_err(|e| e.to_string())?;
    views.reverse();
    let mut previous = None;
    for view in &views {
        let graph = to_graph(store, view).map_err(|e| e.to_string())?;
        write_files(&graph, dir)?;
        commit_all(dir, &commit_message(store, view, previous))?;
        previous = Some(view);
    }
    std::fs::write(dir.join("HANDOFF.md"), handoff).map_err(|e| e.to_string())?;
    commit_all(dir, "handoff report")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Node, Placement, TestOutcome};
    use crate::version::{MemoryStore, Origin, RunId};

    fn func(name: &str, body: Option<&str>) -> Node {
        Node {
            id: NodeId(name.into()),
            kind: NodeKind::Function,
            signature: format!("fn {name}()"),
            body: body.map(Into::into),
            impl_header: None,
            placement: Placement {
                file: "src/lib.rs".into(),
                index: 0,
            },
        }
    }

    fn graph() -> Graph {
        let mut g = Graph::default();
        for (name, body) in [("leaf", Some("{}")), ("mid", Some("{}")), ("top", None)] {
            g.nodes.insert(NodeId(name.into()), func(name, body));
        }
        g.uses
            .entry(NodeId("mid".into()))
            .or_default()
            .insert(NodeId("leaf".into()));
        g.uses
            .entry(NodeId("top".into()))
            .or_default()
            .insert(NodeId("mid".into()));
        let mut test = func("test_mid", Some("{}"));
        test.kind = NodeKind::Test;
        g.nodes.insert(test.id.clone(), test);
        g.uses
            .entry(NodeId("test_mid".into()))
            .or_default()
            .insert(NodeId("mid".into()));
        g
    }

    fn failing_report() -> TestReport {
        TestReport {
            compiled: true,
            outcomes: vec![TestOutcome {
                test: NodeId("test_mid".into()),
                passed: false,
                message: "\nassertion failed: left == right".into(),
            }],
            build_errors: BTreeMap::new(),
            unattributed: Vec::new(),
        }
    }

    fn by_name<'a>(reports: &'a [NodeReport], name: &str) -> &'a NodeReport {
        reports.iter().find(|r| r.node.0 == name).unwrap()
    }

    #[test]
    fn statuses_and_depths() {
        let g = graph();
        let mut failures = BTreeMap::new();
        failures.insert(NodeId("top".into()), "type mismatch\nmore".to_string());
        let reports = node_reports(&g, &failing_report(), &failures, &MemoryStore::default());
        assert_eq!(by_name(&reports, "leaf").status, Status::Passing);
        assert_eq!(by_name(&reports, "leaf").depth, 0);
        assert_eq!(by_name(&reports, "mid").status, Status::FailingTests);
        assert_eq!(by_name(&reports, "mid").depth, 1);
        assert_eq!(
            by_name(&reports, "mid").last_error.as_deref(),
            Some("assertion failed: left == right")
        );
        assert_eq!(by_name(&reports, "top").status, Status::Unfilled);
        assert_eq!(by_name(&reports, "top").depth, 2);
        assert_eq!(
            by_name(&reports, "top").last_error.as_deref(),
            Some("type mismatch")
        );
        assert_eq!(reports.len(), 3);
    }

    #[test]
    fn compile_error_is_attributed() {
        let g = graph();
        let mut report = failing_report();
        report.compiled = false;
        report.outcomes.clear();
        report
            .build_errors
            .insert(NodeId("leaf".into()), vec!["error[E0308]: mismatched".into()]);
        let reports = node_reports(&g, &report, &BTreeMap::new(), &MemoryStore::default());
        assert_eq!(by_name(&reports, "leaf").status, Status::DoesNotCompile);
        assert_eq!(by_name(&reports, "mid").status, Status::Unverified);
    }

    #[test]
    fn start_here_skips_nodes_blocked_by_a_failing_dependency() {
        let g = graph();
        let reports = node_reports(&g, &failing_report(), &BTreeMap::new(), &MemoryStore::default());
        assert_eq!(start_here(&g, &reports), vec![NodeId("mid".into())]);
        let text = render(&g, &reports);
        assert!(text.contains("1 of 3 functions pass"));
        assert!(text.contains("- `mid`"));
        assert!(text.contains("| `top` | unfilled (todo!) | 2 | 0 |"));
        assert!(!text.contains("## Structure"));
    }

    #[test]
    fn long_function_is_listed_under_structure() {
        let mut g = graph();
        let lines: String = (0..32).map(|i| format!("let v{i} = {i};\n")).collect();
        g.nodes.get_mut(&NodeId("leaf".into())).unwrap().body = Some(format!("{{\n{lines} v0 }}"));
        let reports = node_reports(&g, &failing_report(), &BTreeMap::new(), &MemoryStore::default());
        assert_eq!(by_name(&reports, "leaf").structure.len(), 1);
        let text = render(&g, &reports);
        assert!(text.contains("## Structure"));
        assert!(text.contains("- `leaf`: 33 lines of code (limit 30)"));
    }

    #[test]
    fn repo_has_one_commit_per_view_and_a_report() {
        let dir = std::env::temp_dir().join(format!("sg-handoff-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let origin = Origin {
            run: RunId("r".into()),
            model: "m".into(),
            prompt_hash: String::new(),
            created_ms: 0,
        };
        let mut store = MemoryStore::default();
        let mut g = graph();
        g.nodes.get_mut(&NodeId("top".into())).unwrap().body = None;
        let first = crate::view::from_graph(&mut store, &g, &RunId("r".into()), &origin, None).unwrap();
        store.put_view(first.clone()).unwrap();
        g.nodes.get_mut(&NodeId("top".into())).unwrap().body = Some("{ mid() }".into());
        let second =
            crate::view::from_graph(&mut store, &g, &RunId("r".into()), &origin, Some(&first)).unwrap();
        store.put_view(second.clone()).unwrap();

        write_repo(&store, &second.id, None, &dir, "# Handoff\n").unwrap();

        let log = std::process::Command::new("git")
            .args(["log", "--format=%s"])
            .current_dir(&dir)
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&log.stdout);
        assert_eq!(log.lines().count(), 3, "{log}");
        assert!(log.contains("handoff report"));
        assert!(log.contains("nodes changed"));
        assert!(dir.join("HANDOFF.md").exists());
        assert!(std::fs::read_to_string(dir.join("src/lib.rs")).unwrap().contains("mid()"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
