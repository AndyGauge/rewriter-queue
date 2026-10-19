use crate::export::export_view;
use crate::ledger::{self, Status};
use crate::model::{Graph, NodeId, NodeKind, TestOutcome, TestReport};
use crate::variants::TournamentReport;
use crate::version::{Store, VersionId, View, ViewId};
use crate::view;
use std::collections::BTreeSet;
use std::fmt::Write;
use std::path::Path;

pub const MIN_PREFIX: usize = 6;
const SHORT: usize = 12;
const MAX_DIFF_CELLS: usize = 250_000;

fn short(id: &str) -> &str {
    id.get(..SHORT).unwrap_or(id)
}

fn resolve<'a>(
    candidates: impl Iterator<Item = &'a str>,
    input: &str,
    what: &str,
) -> Result<String, String> {
    let all: Vec<&str> = candidates.collect();
    if all.contains(&input) {
        return Ok(input.to_string());
    }
    if input.len() < MIN_PREFIX {
        return Err(format!(
            "{what} id '{input}' is too short: give at least {MIN_PREFIX} characters"
        ));
    }
    let hits: Vec<&str> = all.into_iter().filter(|c| c.starts_with(input)).collect();
    match hits.as_slice() {
        [one] => Ok((*one).to_string()),
        [] => Err(format!("no {what} matches '{input}'")),
        _ => Err(format!("{what} id '{input}' is ambiguous")),
    }
}

pub fn resolve_view(store: &dyn Store, input: &str) -> Result<ViewId, String> {
    let views = store.list_views().map_err(|e| e.to_string())?;
    resolve(views.iter().map(|v| v.0.as_str()), input, "view").map(ViewId)
}

pub fn resolve_version(store: &dyn Store, node: &NodeId, input: &str) -> Result<VersionId, String> {
    let versions = store.versions_of(node).map_err(|e| e.to_string())?;
    resolve(versions.iter().map(|v| v.0.as_str()), input, "version").map(VersionId)
}

fn counts(store: &dyn Store, view: &ViewId) -> Result<Option<(usize, usize)>, String> {
    let Some(results) = store.results_for(view).map_err(|e| e.to_string())? else {
        return Ok(None);
    };
    let pass = results.tests.iter().filter(|t| t.passed).count();
    Ok(Some((pass, results.tests.len() - pass)))
}

fn describe(counts: Option<(usize, usize)>) -> String {
    counts.map_or("no results".to_string(), |(p, f)| {
        format!("{p} pass, {f} fail")
    })
}

pub fn history(store: &dyn Store, node: Option<&NodeId>) -> Result<String, String> {
    let mut out = String::new();
    let views = store.list_views().map_err(|e| e.to_string())?;
    let Some(node) = node else {
        for id in views.iter().rev() {
            let v = store.get_view(id).map_err(|e| e.to_string())?;
            let parent = v.parent.as_ref().map_or("-", |p| short(&p.0));
            let _ = writeln!(
                out,
                "{}  run {}  parent {}  {}",
                short(&id.0),
                v.run.0,
                parent,
                describe(counts(store, id)?)
            );
        }
        return Ok(out);
    };
    let ids = store.versions_of(node).map_err(|e| e.to_string())?;
    if ids.is_empty() {
        return Err(format!("no versions of {}", node.0));
    }
    let mut loaded = Vec::new();
    for view_id in &views {
        loaded.push(store.get_view(view_id).map_err(|e| e.to_string())?);
    }
    for id in &ids {
        let v = store.get_version(id).map_err(|e| e.to_string())?;
        let parent = v.parent.as_ref().map_or("-", |p| short(&p.0));
        let _ = writeln!(
            out,
            "{}  run {}  parent {}",
            short(&id.0),
            v.origin.run.0,
            parent
        );
        for view in loaded.iter().filter(|w| w.versions.get(node) == Some(id)) {
            let _ = writeln!(
                out,
                "    view {}  {}",
                short(&view.id.0),
                describe(counts(store, &view.id)?)
            );
        }
    }
    Ok(out)
}

fn line_diff(a: &str, b: &str) -> Option<Vec<String>> {
    let a: Vec<&str> = a.lines().collect();
    let b: Vec<&str> = b.lines().collect();
    if a.len().saturating_mul(b.len()) > MAX_DIFF_CELLS {
        return None;
    }
    let mut lcs = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let (mut i, mut j) = (0, 0);
    let mut out = Vec::new();
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            i += 1;
            j += 1;
        } else if j == b.len() || (i < a.len() && lcs[i + 1][j] >= lcs[i][j + 1]) {
            out.push(format!("- {}", a[i]));
            i += 1;
        } else {
            out.push(format!("+ {}", b[j]));
            j += 1;
        }
    }
    Some(out)
}

fn lines(text: Option<&str>) -> usize {
    text.map_or(0, |t| t.lines().count())
}

fn diff_part(out: &mut String, label: &str, a: Option<&str>, b: Option<&str>) {
    if a == b {
        return;
    }
    let _ = writeln!(out, "  {label}: {} -> {} lines", lines(a), lines(b));
    if let Some(changes) = line_diff(a.unwrap_or(""), b.unwrap_or("")) {
        for line in changes {
            let _ = writeln!(out, "    {line}");
        }
    }
}

pub fn diff_views(store: &dyn Store, a: &str, b: &str) -> Result<String, String> {
    let a = store
        .get_view(&resolve_view(store, a)?)
        .map_err(|e| e.to_string())?;
    let b = store
        .get_view(&resolve_view(store, b)?)
        .map_err(|e| e.to_string())?;
    let changed = view::diff(&a, &b);
    let mut out = String::new();
    if changed.is_empty() {
        out.push_str("no differences\n");
        return Ok(out);
    }
    for node in changed {
        let old = a
            .versions
            .get(&node)
            .map(|v| store.get_version(v))
            .transpose()
            .map_err(|e| e.to_string())?;
        let new = b
            .versions
            .get(&node)
            .map(|v| store.get_version(v))
            .transpose()
            .map_err(|e| e.to_string())?;
        match (old, new) {
            (Some(o), Some(n)) => {
                let _ = writeln!(
                    out,
                    "changed {}  {} -> {}",
                    node.0,
                    short(&o.id.0),
                    short(&n.id.0)
                );
                diff_part(
                    &mut out,
                    "signature",
                    Some(&o.signature),
                    Some(&n.signature),
                );
                diff_part(&mut out, "body", o.body.as_deref(), n.body.as_deref());
            }
            (None, Some(n)) => {
                let _ = writeln!(
                    out,
                    "added {}  {}  signature {} lines, body {} lines",
                    node.0,
                    short(&n.id.0),
                    lines(Some(&n.signature)),
                    lines(n.body.as_deref())
                );
            }
            (Some(o), None) => {
                let _ = writeln!(
                    out,
                    "removed {}  {}  signature {} lines, body {} lines",
                    node.0,
                    short(&o.id.0),
                    lines(Some(&o.signature)),
                    lines(o.body.as_deref())
                );
            }
            (None, None) => {}
        }
    }
    Ok(out)
}

pub fn undo(store: &dyn Store, view: &str) -> Result<ViewId, String> {
    let id = resolve_view(store, view)?;
    let v = store.get_view(&id).map_err(|e| e.to_string())?;
    view::undo(store, &v)
        .map_err(|e| e.to_string())?
        .map(|p| p.id)
        .ok_or_else(|| format!("view {} has no parent", short(&id.0)))
}

pub fn swap_cmd(
    store: &mut dyn Store,
    view: &str,
    node: &NodeId,
    version: &str,
    created_ms: u64,
) -> Result<ViewId, String> {
    let id = resolve_view(store, view)?;
    let base = store.get_view(&id).map_err(|e| e.to_string())?;
    let version = resolve_version(store, node, version)?;
    let swapped = view::swap(&base, node, version, created_ms).map_err(|e| e.to_string())?;
    store.put_view(swapped).map_err(|e| e.to_string())
}

pub fn export_cmd(
    store: &dyn Store,
    view: &str,
    dir: &Path,
    commit: bool,
) -> Result<ViewId, String> {
    let id = resolve_view(store, view)?;
    let v = store.get_view(&id).map_err(|e| e.to_string())?;
    export_view(store, &v, dir, commit)?;
    Ok(id)
}

pub fn parse_node(graph_view: &View, input: &str) -> Result<NodeId, String> {
    let id = NodeId(input.to_string());
    if graph_view.versions.contains_key(&id) {
        Ok(id)
    } else {
        Err(format!(
            "no node {input} in view {}",
            short(&graph_view.id.0)
        ))
    }
}

/// Bench nodes that use `node` directly or through other nodes.
pub fn benches_reaching(graph: &Graph, node: &NodeId) -> Vec<NodeId> {
    let mut reach: BTreeSet<NodeId> = BTreeSet::from([node.clone()]);
    loop {
        let before = reach.len();
        for (from, deps) in &graph.uses {
            if deps.iter().any(|d| reach.contains(d)) {
                reach.insert(from.clone());
            }
        }
        if reach.len() == before {
            break;
        }
    }
    reach
        .into_iter()
        .filter(|id| {
            graph
                .nodes
                .get(id)
                .is_some_and(|n| n.kind == NodeKind::Bench)
        })
        .collect()
}

/// Test outcomes with compile errors counted as failures, as stored for a view.
pub fn outcomes_of(report: &TestReport) -> Vec<TestOutcome> {
    let mut out = report.outcomes.clone();
    for (node, errs) in &report.build_errors {
        for e in errs {
            out.push(TestOutcome {
                test: node.clone(),
                passed: false,
                message: e.clone(),
            });
        }
    }
    for e in &report.unattributed {
        out.push(TestOutcome {
            test: NodeId(String::new()),
            passed: false,
            message: e.clone(),
        });
    }
    out
}

pub fn format_report(report: &TournamentReport) -> String {
    let mut out = String::new();
    match &report.baseline {
        Some(b) => {
            let _ = writeln!(out, "baseline: {} ns (spread {})", b.median_ns, b.spread_ns);
        }
        None => out.push_str("baseline: no bench result\n"),
    }
    for c in &report.candidates {
        let timing = c.bench.as_ref().map_or("no bench".to_string(), |b| {
            format!("{} ns (spread {})", b.median_ns, b.spread_ns)
        });
        let _ = writeln!(
            out,
            "variant {}: {}  {}",
            short(&c.version.0),
            if c.passed { "tests pass" } else { "tests fail" },
            timing
        );
    }
    match (&report.winner, &report.view) {
        (Some(w), Some(v)) => {
            let _ = writeln!(out, "winner: {}  view {}", short(&w.0), v.id.0);
        }
        _ => out.push_str("no variant beat the baseline\n"),
    }
    out
}

fn version_label(node: &NodeId, version: &VersionId) -> String {
    format!("{}@{}", node.0, short(&version.0))
}

/// The failure ledger as text, optionally for one node and optionally only the
/// failures still open.
pub fn failures(store: &dyn Store, node: Option<&NodeId>, open_only: bool) -> Result<String, String> {
    let all = ledger::ledger(store).map_err(|e| e.to_string())?;
    let mut out = String::new();
    for f in all
        .iter()
        .filter(|f| !open_only || f.is_open())
        .filter(|f| node.is_none_or(|n| f.versions.contains_key(n) || &f.test == n))
    {
        let seen: Vec<String> = f
            .versions
            .iter()
            .flat_map(|(n, vs)| vs.iter().map(move |v| version_label(n, v)))
            .collect();
        let state = match &f.status {
            Status::Open => "open".to_string(),
            Status::Resolved { by, in_view } => format!(
                "resolved in view {}{}",
                short(&in_view.0),
                if by.is_empty() {
                    " with no version change".to_string()
                } else {
                    " by a new version".to_string()
                }
            ),
        };
        let _ = writeln!(
            out,
            "{} [{state}] {}: {} (seen {} times)\n    versions: {}",
            short(&f.id),
            f.test.0,
            f.summary,
            f.occurrences,
            seen.join(", ")
        );
    }
    if out.is_empty() {
        out.push_str("no failures\n");
    }
    Ok(out)
}

/// The newest version of a node that compiled, and whether its tests passed.
pub fn good(store: &dyn Store, node: &NodeId) -> Result<String, String> {
    match ledger::last_good(store, node).map_err(|e| e.to_string())? {
        None => Ok(format!("{}: no version has compiled\n", node.0)),
        Some(g) => Ok(format!(
            "{}: {} in view {} ({})\n",
            node.0,
            short(&g.version.0),
            short(&g.view.0),
            match (g.passing, g.tested) {
                (true, true) => "compiled, tests pass",
                (true, false) => "compiled, no test reaches it",
                (false, _) => "compiled, tests fail",
            }
        )),
    }
}

/// Move every function with an open failure, or no body, back to its last good
/// version. With `apply` false only the plan is returned.
pub fn sweep(
    store: &mut dyn Store,
    view: &str,
    apply: bool,
    created_ms: u64,
) -> Result<(String, Option<ViewId>), String> {
    let id = resolve_view(store, view)?;
    let base = store.get_view(&id).map_err(|e| e.to_string())?;
    let plan = ledger::sweep_plan(store, &base).map_err(|e| e.to_string())?;
    if plan.is_empty() {
        return Ok(("nothing to sweep\n".to_string(), None));
    }
    let mut out = String::new();
    for (node, version) in &plan {
        let _ = writeln!(out, "{} -> {}", node.0, short(&version.0));
    }
    if !apply {
        return Ok((out, None));
    }
    let swapped = view::swap_many(&base, &plan, created_ms).map_err(|e| e.to_string())?;
    let new = store.put_view(swapped).map_err(|e| e.to_string())?;
    let _ = writeln!(out, "new view {}", short(&new.0));
    Ok((out, Some(new)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Node, Placement};
    use crate::variants::Candidate;
    use crate::version::{BenchResult, MemoryStore, Origin, Results, RunId};
    use std::collections::BTreeMap;

    fn origin(run: &str) -> Origin {
        Origin {
            run: RunId(run.into()),
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
                file: "src/lib.rs".into(),
                index: 0,
            },
        }
    }

    fn graph(bodies: &[(&str, &str)]) -> Graph {
        let mut g = Graph::default();
        for (n, b) in bodies {
            g.insert(node(n, b));
        }
        g
    }

    fn add(s: &mut MemoryStore, g: &Graph, run: &str, parent: Option<&View>) -> View {
        let v = view::from_graph(s, g, &RunId(run.into()), &origin(run), parent).unwrap();
        s.put_view(v.clone()).unwrap();
        v
    }

    fn record(s: &mut MemoryStore, v: &View, passed: &[bool]) {
        s.put_results(Results {
            view: v.id.clone(),
            tests: passed
                .iter()
                .enumerate()
                .map(|(i, p)| TestOutcome {
                    test: NodeId(format!("t{i}")),
                    passed: *p,
                    message: String::new(),
                })
                .collect(),
            benches: vec![],
            clippy_clean: None,
            recorded_ms: 1,
        })
        .unwrap();
    }

    fn two_views() -> (MemoryStore, View, View) {
        let mut s = MemoryStore::default();
        let v1 = add(
            &mut s,
            &graph(&[("a", "{ 1 }"), ("b", "{ 2 }")]),
            "r1",
            None,
        );
        record(&mut s, &v1, &[true, false]);
        let v2 = add(
            &mut s,
            &graph(&[("a", "{ 1 }"), ("b", "{\n 2 + 0\n}")]),
            "r2",
            Some(&v1),
        );
        record(&mut s, &v2, &[true, true]);
        (s, v1, v2)
    }

    #[test]
    fn history_lists_views_newest_first_with_counts() {
        let (s, v1, v2) = two_views();
        let text = history(&s, None).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with(short(&v2.id.0)));
        assert!(lines[0].contains("run r2"));
        assert!(lines[0].contains(short(&v1.id.0)));
        assert!(lines[0].contains("2 pass, 0 fail"));
        assert!(lines[1].contains("parent -"));
        assert!(lines[1].contains("1 pass, 1 fail"));
    }

    #[test]
    fn history_for_node_lists_versions_oldest_first_with_views() {
        let (s, v1, v2) = two_views();
        let b = NodeId("b".into());
        let text = history(&s, Some(&b)).unwrap();
        let first = short(&v1.versions[&b].0).to_string();
        let second = short(&v2.versions[&b].0).to_string();
        let i = text.find(&first).unwrap();
        let j = text.find(&second).unwrap();
        assert!(i < j);
        assert!(text.contains(&format!("view {}  1 pass, 1 fail", short(&v1.id.0))));
        assert!(text.contains(&format!("view {}  2 pass, 0 fail", short(&v2.id.0))));
        assert!(history(&s, Some(&NodeId("zz".into()))).is_err());
    }

    #[test]
    fn diff_shows_changed_nodes_only() {
        let (s, v1, v2) = two_views();
        let text = diff_views(&s, &v1.id.0, &v2.id.0).unwrap();
        assert!(text.contains("changed b"));
        assert!(!text.contains("changed a"));
        assert!(text.contains("body: 1 -> 3 lines"));
        assert!(text.contains("- { 2 }"));
        assert!(text.contains("+  2 + 0"));
        assert_eq!(
            diff_views(&s, &v1.id.0, &v1.id.0).unwrap(),
            "no differences\n"
        );
    }

    #[test]
    fn diff_reports_added_and_removed() {
        let mut s = MemoryStore::default();
        let v1 = add(&mut s, &graph(&[("a", "{ 1 }"), ("b", "{ 2 }")]), "r", None);
        let v2 = add(
            &mut s,
            &graph(&[("a", "{ 1 }"), ("c", "{ 3 }")]),
            "r",
            Some(&v1),
        );
        let text = diff_views(&s, &v1.id.0, &v2.id.0).unwrap();
        assert!(text.contains("removed b"));
        assert!(text.contains("added c"));
    }

    #[test]
    fn undo_returns_parent_or_errors() {
        let (s, v1, v2) = two_views();
        assert_eq!(undo(&s, &v2.id.0).unwrap(), v1.id);
        assert!(undo(&s, &v1.id.0).unwrap_err().contains("no parent"));
    }

    #[test]
    fn swap_stores_new_view_pointing_at_base() {
        let (mut s, v1, v2) = two_views();
        let b = NodeId("b".into());
        let old = v1.versions[&b].0.clone();
        let new = swap_cmd(&mut s, &v2.id.0, &b, &old[..8], 5).unwrap();
        let stored = s.get_view(&new).unwrap();
        assert_eq!(stored.versions[&b], v1.versions[&b]);
        assert_eq!(stored.parent, Some(v2.id.clone()));
        assert!(s.list_views().unwrap().contains(&new));
        let other = swap_cmd(&mut s, &v2.id.0, &NodeId("a".into()), &old, 5);
        assert!(other.is_err());
    }

    #[test]
    fn prefixes_need_six_characters_and_must_be_unique() {
        let (s, v1, _) = two_views();
        let full = &v1.id.0;
        assert_eq!(resolve_view(&s, &full[..6]).unwrap(), v1.id);
        assert!(resolve_view(&s, &full[..5])
            .unwrap_err()
            .contains("too short"));
        assert!(resolve_view(&s, "ffffffff")
            .unwrap_err()
            .contains("no view"));
    }

    #[test]
    fn export_renders_view_to_directory() {
        let (s, v1, _) = two_views();
        let dir = std::env::temp_dir().join(format!("sg-cli-export-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let id = export_cmd(&s, &v1.id.0[..8], &dir, false).unwrap();
        assert_eq!(id, v1.id);
        let src = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap();
        assert!(src.contains("fn a()"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn benches_reaching_follows_uses() {
        let mut g = graph(&[("f", "{ 1 }"), ("g", "{ f() }")]);
        let mut bench = node("bench_g", "{ g(); }");
        bench.kind = NodeKind::Bench;
        g.insert(bench);
        g.add_use(&NodeId("g".into()), &NodeId("f".into()));
        g.add_use(&NodeId("bench_g".into()), &NodeId("g".into()));
        assert_eq!(
            benches_reaching(&g, &NodeId("f".into())),
            vec![NodeId("bench_g".into())]
        );
        assert!(benches_reaching(&g, &NodeId("bench_g".into())).len() == 1);
    }

    #[test]
    fn outcomes_include_build_errors() {
        let report = TestReport {
            compiled: false,
            outcomes: vec![],
            build_errors: BTreeMap::from([(NodeId("f".into()), vec!["bad".into()])]),
            unattributed: vec!["worse".into()],
        };
        let out = outcomes_of(&report);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|o| !o.passed));
    }

    #[test]
    fn report_names_winner_view() {
        let (_, _, v2) = two_views();
        let bench = |ns| BenchResult {
            bench: NodeId("bench".into()),
            median_ns: ns,
            spread_ns: 1,
            runs: 5,
            instructions: None,
        };
        let report = TournamentReport {
            baseline: Some(bench(100)),
            candidates: vec![Candidate {
                version: VersionId("abcdef0123456789".into()),
                passed: true,
                bench: Some(bench(40)),
            }],
            winner: Some(VersionId("abcdef0123456789".into())),
            view: Some(v2.clone()),
        };
        let text = format_report(&report);
        assert!(text.contains("baseline: 100 ns"));
        assert!(text.contains("tests pass  40 ns"));
        assert!(text.contains(&format!("view {}", v2.id.0)));
    }
}
