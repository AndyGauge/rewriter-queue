use crate::model::{NodeId, NodeKind};
use crate::version::{content_hash, Store, StoreError, VersionId, View, ViewId};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The crate did not compile and the error was attributed to a node.
    Build,
    /// A test failed.
    Test,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Open,
    /// A later view no longer shows it. `by` are the versions of the affected
    /// nodes that differ from the last view in which it appeared (empty when
    /// nothing changed, which points at a flaky test).
    Resolved { by: Vec<VersionId>, in_view: ViewId },
}

/// One distinct failure, tracked across views. Resolved failures stay in the
/// ledger, and one that comes back is reopened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    pub id: String,
    pub kind: Kind,
    /// The failing test node, or the node that did not compile.
    pub test: NodeId,
    pub summary: String,
    /// Edges from the failure to the node versions it was seen with.
    pub versions: BTreeMap<NodeId, BTreeSet<VersionId>>,
    pub first_view: ViewId,
    pub last_view: ViewId,
    pub occurrences: usize,
    pub status: Status,
}

impl Failure {
    pub fn is_open(&self) -> bool {
        self.status == Status::Open
    }

    pub fn touches(&self, node: &NodeId, version: &VersionId) -> bool {
        self.versions
            .get(node)
            .is_some_and(|versions| versions.contains(version))
    }
}

/// The text of a failure with the parts that change between runs removed: digit
/// runs (thread ids, line and column numbers) and the backtrace hint.
fn normalise(message: &str) -> String {
    let mut out = Vec::new();
    for line in message.lines().map(str::trim) {
        if line.is_empty() || line.starts_with("note: run with") {
            continue;
        }
        let mut flat = String::new();
        let mut in_digits = false;
        for c in line.chars() {
            if c.is_ascii_digit() {
                if !in_digits {
                    flat.push('#');
                }
                in_digits = true;
            } else {
                in_digits = false;
                flat.push(c);
            }
        }
        out.push(flat);
        if out.len() == 4 {
            break;
        }
    }
    out.join("\n")
}

fn summary(message: &str) -> String {
    let line = message
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with("thread '") && !l.starts_with("note: run with"))
        .or_else(|| message.lines().map(str::trim).find(|l| !l.is_empty()))
        .unwrap_or_default();
    if line.chars().count() > 160 {
        format!("{}...", line.chars().take(157).collect::<String>())
    } else {
        line.to_string()
    }
}

struct Kinds<'a> {
    store: &'a dyn Store,
    cache: BTreeMap<VersionId, NodeKind>,
}

impl Kinds<'_> {
    fn of(&mut self, view: &View, node: &NodeId) -> Result<Option<NodeKind>, StoreError> {
        let Some(version) = view.versions.get(node) else {
            return Ok(None);
        };
        if let Some(kind) = self.cache.get(version) {
            return Ok(Some(*kind));
        }
        let kind = self.store.get_version(version)?.kind;
        self.cache.insert(version.clone(), kind);
        Ok(Some(kind))
    }
}

/// What a view's results say, with failing outcomes split into compile failures
/// (the outcome's id is not a test node) and test failures.
struct Reading {
    /// (failing outcome id, is a compile failure, message)
    failing: Vec<(NodeId, bool, String)>,
    tested: BTreeSet<NodeId>,
    any_build_error: bool,
    has_outcomes: bool,
}

fn read(
    kinds: &mut Kinds<'_>,
    view: &View,
    results: &crate::version::Results,
) -> Result<Reading, StoreError> {
    let mut reading = Reading {
        failing: Vec::new(),
        tested: BTreeSet::new(),
        any_build_error: false,
        has_outcomes: !results.tests.is_empty(),
    };
    for outcome in &results.tests {
        let is_test = kinds.of(view, &outcome.test)? == Some(NodeKind::Test);
        if is_test {
            reading.tested.insert(outcome.test.clone());
        }
        if !outcome.passed {
            if !is_test {
                reading.any_build_error = true;
            }
            reading
                .failing
                .push((outcome.test.clone(), !is_test, outcome.message.clone()));
        }
    }
    Ok(reading)
}

fn subjects(kinds: &mut Kinds<'_>, view: &View, outcome: &NodeId, build: bool) -> Result<Vec<NodeId>, StoreError> {
    if build {
        return Ok(vec![outcome.clone()]);
    }
    let mut out = Vec::new();
    for dep in view.uses.get(outcome).into_iter().flatten() {
        if kinds.of(view, dep)? != Some(NodeKind::Test) && view.versions.contains_key(dep) {
            out.push(dep.clone());
        }
    }
    Ok(out)
}

/// Every distinct failure in the store's history, oldest first.
pub fn ledger(store: &dyn Store) -> Result<Vec<Failure>, StoreError> {
    let mut kinds = Kinds {
        store,
        cache: BTreeMap::new(),
    };
    let mut failures: Vec<Failure> = Vec::new();
    let mut last_seen: BTreeMap<String, BTreeMap<NodeId, VersionId>> = BTreeMap::new();

    for view_id in store.list_views()? {
        let Some(results) = store.results_for(&view_id)? else {
            continue;
        };
        let view = store.get_view(&view_id)?;
        let reading = read(&mut kinds, &view, &results)?;

        let mut present: BTreeSet<String> = BTreeSet::new();
        for (test, build, message) in &reading.failing {
            let id = content_hash(&[&test.0, &normalise(message)]);
            present.insert(id.clone());
            let seen = subjects(&mut kinds, &view, test, *build)?;
            let slot = match failures.iter().position(|f| f.id == id) {
                Some(i) => i,
                None => {
                    failures.push(Failure {
                        id: id.clone(),
                        kind: if *build { Kind::Build } else { Kind::Test },
                        test: test.clone(),
                        summary: summary(message),
                        versions: BTreeMap::new(),
                        first_view: view_id.clone(),
                        last_view: view_id.clone(),
                        occurrences: 0,
                        status: Status::Open,
                    });
                    failures.len() - 1
                }
            };
            let f = &mut failures[slot];
            f.occurrences += 1;
            f.last_view = view_id.clone();
            f.status = Status::Open;
            let at = last_seen.entry(id).or_default();
            for node in seen {
                if let Some(version) = view.versions.get(&node) {
                    f.versions
                        .entry(node.clone())
                        .or_default()
                        .insert(version.clone());
                    at.insert(node, version.clone());
                }
            }
        }

        let failing_tests: BTreeSet<&NodeId> = reading.failing.iter().map(|(t, _, _)| t).collect();
        for f in failures.iter_mut().filter(|f| f.is_open() && !present.contains(&f.id)) {
            let gone = match f.kind {
                Kind::Test => reading.tested.contains(&f.test) || failing_tests.contains(&f.test),
                Kind::Build => reading.has_outcomes && !reading.any_build_error,
            };
            if !gone {
                continue;
            }
            let before = last_seen.get(&f.id).cloned().unwrap_or_default();
            let by = before
                .iter()
                .filter_map(|(node, old)| {
                    view.versions.get(node).filter(|now| *now != old).cloned()
                })
                .collect();
            f.status = Status::Resolved {
                by,
                in_view: view_id.clone(),
            };
        }
    }
    if let Some(last) = store.list_views()?.pop() {
        let view = store.get_view(&last)?;
        for f in failures
            .iter_mut()
            .filter(|f| f.is_open() && f.kind == Kind::Test && !view.versions.contains_key(&f.test))
        {
            f.status = Status::Resolved {
                by: Vec::new(),
                in_view: last.clone(),
            };
        }
    }
    Ok(failures)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Good {
    pub version: VersionId,
    pub view: ViewId,
    /// Compiled, and every test reaching the node passed in that view.
    pub passing: bool,
    /// At least one test reached the node in that view.
    pub tested: bool,
}

/// The newest version of `node` that compiled, preferring the newest one whose
/// tests passed. A version with no body is never good.
pub fn last_good(store: &dyn Store, node: &NodeId) -> Result<Option<Good>, StoreError> {
    let mut kinds = Kinds {
        store,
        cache: BTreeMap::new(),
    };
    let mut compiled_only: Option<Good> = None;
    for view_id in store.list_views()?.into_iter().rev() {
        let Some(results) = store.results_for(&view_id)? else {
            continue;
        };
        let view = store.get_view(&view_id)?;
        let Some(version) = view.versions.get(node) else {
            continue;
        };
        if store.get_version(version)?.body.is_none() {
            continue;
        }
        let reading = read(&mut kinds, &view, &results)?;
        if !reading.has_outcomes || reading.any_build_error {
            continue;
        }
        let mut reached = false;
        let mut failed = false;
        for outcome in &results.tests {
            let reaches = subjects(&mut kinds, &view, &outcome.test, false)?.contains(node);
            reached |= reaches;
            failed |= reaches && !outcome.passed;
        }
        let good = Good {
            version: version.clone(),
            view: view_id.clone(),
            passing: !failed,
            tested: reached,
        };
        if good.passing && good.tested {
            return Ok(Some(good));
        }
        if compiled_only.is_none() {
            compiled_only = Some(good);
        }
    }
    Ok(compiled_only)
}

/// Swaps that would move every function or method with an open failure, or with
/// no body, back to its last good version. Only versions whose tests passed are
/// offered for a node that has an open failure; an unfilled node takes any
/// compiled version.
pub fn sweep_plan(store: &dyn Store, view: &View) -> Result<Vec<(NodeId, VersionId)>, StoreError> {
    let failures = ledger(store)?;
    let mut plan = Vec::new();
    for (node, version) in &view.versions {
        let current = store.get_version(version)?;
        if !matches!(current.kind, NodeKind::Function | NodeKind::Method) {
            continue;
        }
        let unfilled = current.body.is_none();
        let failing = failures.iter().any(|f| f.is_open() && f.touches(node, version));
        if !unfilled && !failing {
            continue;
        }
        if let Some(good) = last_good(store, node)? {
            if good.version != *version && (good.passing || unfilled) {
                plan.push((node.clone(), good.version));
            }
        }
    }
    Ok(plan)
}

pub fn summary_markdown(failures: &[Failure]) -> String {
    let open: Vec<&Failure> = failures.iter().filter(|f| f.is_open()).collect();
    let resolved = failures.len() - open.len();
    let mut out = String::from("## Failures\n\n");
    out.push_str(&format!(
        "{} distinct failures were seen: {} still open, {resolved} resolved along the way and kept in the history.\n\n",
        failures.len(),
        open.len()
    ));
    if !open.is_empty() {
        out.push_str("Open:\n\n");
        for f in open {
            let nodes: Vec<&str> = f.versions.keys().map(|n| n.0.as_str()).collect();
            out.push_str(&format!(
                "- `{}`: {} (seen {} times, with `{}`)\n",
                f.test.0,
                f.summary.replace('|', "\\|"),
                f.occurrences,
                nodes.join("`, `")
            ));
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Node, Placement, TestOutcome};
    use crate::version::{MemoryStore, NodeVersion, Origin, Results, RunId};

    fn origin() -> Origin {
        Origin {
            run: RunId("r".into()),
            model: "m".into(),
            prompt_hash: String::new(),
            created_ms: 0,
        }
    }

    fn node(name: &str, kind: NodeKind, body: Option<&str>) -> Node {
        Node {
            id: NodeId(name.into()),
            kind,
            signature: format!("fn {name}()"),
            body: body.map(Into::into),
            impl_header: None,
            placement: Placement {
                file: "src/lib.rs".into(),
                index: 0,
            },
        }
    }

    struct Fixture {
        store: MemoryStore,
        parent: Option<View>,
        clock: u64,
    }

    impl Fixture {
        fn new() -> Self {
            Fixture {
                store: MemoryStore::default(),
                parent: None,
                clock: 0,
            }
        }

        /// A view with `f` at the given body, one test `t_f` reaching it, and the given outcome.
        fn view(&mut self, body: Option<&str>, outcomes: Vec<TestOutcome>) -> (View, VersionId) {
            let f = NodeVersion::new(&node("f", NodeKind::Function, body), None, origin());
            let t = NodeVersion::new(&node("t_f", NodeKind::Test, Some("{}")), None, origin());
            let fid = self.store.put_version(f).unwrap();
            let tid = self.store.put_version(t).unwrap();
            self.clock += 1;
            let mut versions = BTreeMap::new();
            versions.insert(NodeId("f".into()), fid.clone());
            versions.insert(NodeId("t_f".into()), tid);
            let mut uses = BTreeMap::new();
            uses.insert(
                NodeId("t_f".into()),
                BTreeSet::from([NodeId("f".into())]),
            );
            let view = View::new(
                RunId("r".into()),
                self.parent.as_ref().map(|p| p.id.clone()),
                versions,
                uses,
                self.clock,
            );
            self.store.put_view(view.clone()).unwrap();
            self.store
                .put_results(Results {
                    view: view.id.clone(),
                    tests: outcomes,
                    benches: Vec::new(),
                    clippy_clean: None,
                    recorded_ms: self.clock,
                })
                .unwrap();
            self.parent = Some(view.clone());
            (view, fid)
        }
    }

    fn fail(test: &str, message: &str) -> TestOutcome {
        TestOutcome {
            test: NodeId(test.into()),
            passed: false,
            message: message.into(),
        }
    }

    fn pass(test: &str) -> TestOutcome {
        TestOutcome {
            test: NodeId(test.into()),
            passed: true,
            message: String::new(),
        }
    }

    #[test]
    fn a_failure_is_opened_linked_to_versions_and_later_resolved() {
        let mut fx = Fixture::new();
        let (_, bad) = fx.view(Some("{ 1 }"), vec![fail("t_f", "thread 'x' (123) panicked at src/lib.rs:4:9:\nboom")]);
        let (_, bad_again) = fx.view(Some("{ 1 }"), vec![fail("t_f", "thread 'x' (987) panicked at src/lib.rs:4:9:\nboom")]);
        assert_eq!(bad, bad_again);
        let open = ledger(&fx.store).unwrap();
        assert_eq!(open.len(), 1, "thread ids must not make a new failure");
        assert!(open[0].is_open());
        assert_eq!(open[0].occurrences, 2);
        assert!(open[0].touches(&NodeId("f".into()), &bad));
        assert_eq!(open[0].summary, "boom");

        let (view, fixed) = fx.view(Some("{ 2 }"), vec![pass("t_f")]);
        let all = ledger(&fx.store).unwrap();
        assert_eq!(all.len(), 1, "resolved failures stay in the history");
        assert_eq!(
            all[0].status,
            Status::Resolved {
                by: vec![fixed],
                in_view: view.id
            }
        );
    }

    #[test]
    fn a_failure_that_comes_back_is_reopened() {
        let mut fx = Fixture::new();
        fx.view(Some("{ 1 }"), vec![fail("t_f", "boom")]);
        fx.view(Some("{ 2 }"), vec![pass("t_f")]);
        fx.view(Some("{ 3 }"), vec![fail("t_f", "boom")]);
        let all = ledger(&fx.store).unwrap();
        assert_eq!(all.len(), 1);
        assert!(all[0].is_open());
        assert_eq!(all[0].occurrences, 2);
    }

    #[test]
    fn a_different_message_is_a_different_failure_and_the_old_one_resolves() {
        let mut fx = Fixture::new();
        fx.view(Some("{ 1 }"), vec![fail("t_f", "left: 1")]);
        fx.view(Some("{ 2 }"), vec![fail("t_f", "index out of bounds")]);
        let all = ledger(&fx.store).unwrap();
        assert_eq!(all.len(), 2);
        assert!(matches!(all[0].status, Status::Resolved { .. }));
        assert!(all[1].is_open());
    }

    #[test]
    fn a_failure_of_a_test_that_no_longer_exists_is_resolved() {
        let mut fx = Fixture::new();
        let (view, _) = fx.view(Some("{ 1 }"), vec![fail("t_f", "boom")]);
        let mut versions = view.versions.clone();
        versions.remove(&NodeId("t_f".into()));
        let without_test = View::new(
            view.run.clone(),
            Some(view.id.clone()),
            versions,
            BTreeMap::new(),
            99,
        );
        fx.store.put_view(without_test.clone()).unwrap();
        fx.store
            .put_results(Results {
                view: without_test.id.clone(),
                tests: Vec::new(),
                benches: Vec::new(),
                clippy_clean: None,
                recorded_ms: 99,
            })
            .unwrap();
        let all = ledger(&fx.store).unwrap();
        assert!(matches!(all[0].status, Status::Resolved { ref by, .. } if by.is_empty()));
    }

    #[test]
    fn a_test_that_was_not_run_does_not_resolve_its_failure() {
        let mut fx = Fixture::new();
        fx.view(Some("{ 1 }"), vec![fail("t_f", "boom")]);
        fx.view(Some("{ 2 }"), vec![pass("other_test")]);
        assert!(ledger(&fx.store).unwrap()[0].is_open());
    }

    #[test]
    fn last_good_prefers_the_newest_passing_version_and_skips_failing_and_unfilled_ones() {
        let mut fx = Fixture::new();
        fx.view(Some("{ 1 }"), vec![pass("t_f")]);
        let (_, passing) = fx.view(Some("{ 2 }"), vec![pass("t_f")]);
        fx.view(Some("{ 3 }"), vec![fail("t_f", "boom")]);
        fx.view(None, vec![pass("t_f")]);
        let good = last_good(&fx.store, &NodeId("f".into())).unwrap().unwrap();
        assert_eq!(good.version, passing);
        assert!(good.passing && good.tested);
    }

    #[test]
    fn last_good_falls_back_to_a_version_that_only_compiled() {
        let mut fx = Fixture::new();
        let (_, compiled) = fx.view(Some("{ 1 }"), vec![fail("t_f", "boom")]);
        let good = last_good(&fx.store, &NodeId("f".into())).unwrap().unwrap();
        assert_eq!(good.version, compiled);
        assert!(!good.passing);
    }

    #[test]
    fn a_version_that_never_compiled_is_not_good() {
        let mut fx = Fixture::new();
        fx.view(Some("{ oops }"), vec![fail("f", "error[E0425]: cannot find value")]);
        assert_eq!(last_good(&fx.store, &NodeId("f".into())).unwrap(), None);
        let all = ledger(&fx.store).unwrap();
        assert_eq!(all[0].kind, Kind::Build);
    }

    #[test]
    fn sweep_moves_a_failing_node_back_to_its_last_passing_version() {
        let mut fx = Fixture::new();
        let (_, passing) = fx.view(Some("{ 1 }"), vec![pass("t_f")]);
        let (view, _) = fx.view(Some("{ 2 }"), vec![fail("t_f", "boom")]);
        let plan = sweep_plan(&fx.store, &view).unwrap();
        assert_eq!(plan, vec![(NodeId("f".into()), passing)]);
    }

    #[test]
    fn sweep_leaves_a_passing_node_alone_and_does_not_revert_to_a_failing_version() {
        let mut fx = Fixture::new();
        let (passing_view, _) = fx.view(Some("{ 1 }"), vec![pass("t_f")]);
        assert!(sweep_plan(&fx.store, &passing_view).unwrap().is_empty());
        let mut fx = Fixture::new();
        fx.view(Some("{ 1 }"), vec![fail("t_f", "a")]);
        let (view, _) = fx.view(Some("{ 2 }"), vec![fail("t_f", "b")]);
        assert!(sweep_plan(&fx.store, &view).unwrap().is_empty());
    }

    #[test]
    fn sweep_gives_an_unfilled_node_its_last_compiled_version() {
        let mut fx = Fixture::new();
        let (_, compiled) = fx.view(Some("{ 1 }"), vec![fail("t_f", "boom")]);
        let (view, _) = fx.view(None, vec![fail("t_f", "not yet implemented")]);
        let plan = sweep_plan(&fx.store, &view).unwrap();
        assert_eq!(plan, vec![(NodeId("f".into()), compiled)]);
    }

    #[test]
    fn summary_lists_open_failures_and_counts_resolved_ones() {
        let mut fx = Fixture::new();
        fx.view(Some("{ 1 }"), vec![fail("t_f", "left: 1")]);
        fx.view(Some("{ 2 }"), vec![fail("t_f", "index out of bounds")]);
        let text = summary_markdown(&ledger(&fx.store).unwrap());
        assert!(text.contains("2 distinct failures were seen: 1 still open, 1 resolved"));
        assert!(text.contains("index out of bounds"));
        assert!(!text.contains("left: 1"));
    }
}
