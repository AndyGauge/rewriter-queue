use crate::model::NodeId;
use crate::version::{Store, StoreError, VersionId};
use std::collections::BTreeMap;

/// Versions of `node` that appear in a view whose recorded tests all passed and
/// that has a result for `bench`, with that median, fastest first, ties by
/// version id.
pub fn passing_by_bench(
    store: &dyn Store,
    node: &NodeId,
    bench: &NodeId,
) -> Result<Vec<(VersionId, u64)>, StoreError> {
    let mut best: BTreeMap<VersionId, u64> = BTreeMap::new();
    for view_id in store.list_views()? {
        let Some(results) = store.results_for(&view_id)? else {
            continue;
        };
        if !results.tests.iter().all(|t| t.passed) {
            continue;
        }
        let Some(median) = results
            .benches
            .iter()
            .find(|b| &b.bench == bench)
            .map(|b| b.median_ns)
        else {
            continue;
        };
        let view = store.get_view(&view_id)?;
        let Some(version) = view.versions.get(node) else {
            continue;
        };
        best.entry(version.clone())
            .and_modify(|m| *m = (*m).min(median))
            .or_insert(median);
    }
    let mut out: Vec<(VersionId, u64)> = best.into_iter().collect();
    out.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    Ok(out)
}

/// Failure messages recorded for tests that reach `node` in views where it
/// used a version that is not the latest, oldest first, for use in prompts.
pub fn failures_for(store: &dyn Store, node: &NodeId) -> Result<Vec<String>, StoreError> {
    let Some(latest) = store.versions_of(node)?.pop() else {
        return Ok(Vec::new());
    };
    let mut out: Vec<String> = Vec::new();
    for view_id in store.list_views()? {
        let Some(results) = store.results_for(&view_id)? else {
            continue;
        };
        let view = store.get_view(&view_id)?;
        match view.versions.get(node) {
            Some(version) if *version != latest => {}
            _ => continue,
        }
        for outcome in results.tests.iter().filter(|t| !t.passed) {
            let reaches = view
                .uses
                .get(&outcome.test)
                .is_some_and(|subjects| subjects.contains(node));
            if reaches && !out.contains(&outcome.message) {
                out.push(outcome.message.clone());
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Node, NodeId, NodeKind, Placement, TestOutcome};
    use crate::version::{BenchResult, MemoryStore, NodeVersion, Origin, Results, RunId, View};
    use std::collections::BTreeSet;

    fn id(s: &str) -> NodeId {
        NodeId(s.into())
    }

    fn add_version(s: &mut MemoryStore, body: &str) -> VersionId {
        let node = Node {
            id: id("f"),
            kind: NodeKind::Function,
            signature: "fn f()".into(),
            body: Some(body.into()),
            impl_header: None,
            placement: Placement {
                file: "lib.rs".into(),
                index: 0,
            },
        };
        let origin = Origin {
            run: RunId("r".into()),
            model: "m".into(),
            prompt_hash: "p".into(),
            created_ms: 0,
        };
        s.put_version(NodeVersion::new(&node, None, origin))
            .unwrap()
    }

    fn add_view(
        s: &mut MemoryStore,
        version: &VersionId,
        stamp: u64,
        outcomes: &[(&str, bool, &str)],
        bench: Option<u64>,
    ) {
        let uses = outcomes
            .iter()
            .map(|(t, _, _)| (id(t), BTreeSet::from([id("f")])))
            .collect();
        let view = View::new(
            RunId("r".into()),
            None,
            BTreeMap::from([(id("f"), version.clone())]),
            uses,
            stamp,
        );
        let view_id = view.id.clone();
        s.put_view(view).unwrap();
        s.put_results(Results {
            view: view_id,
            tests: outcomes
                .iter()
                .map(|(t, passed, m)| TestOutcome {
                    test: id(t),
                    passed: *passed,
                    message: (*m).into(),
                })
                .collect(),
            benches: bench
                .map(|m| BenchResult {
                    bench: id("bench::f"),
                    median_ns: m,
                    spread_ns: 1,
                    runs: 5,
                    instructions: None,
                })
                .into_iter()
                .collect(),
            clippy_clean: None,
            recorded_ms: stamp,
        })
        .unwrap();
    }

    #[test]
    fn passing_sorted_fastest_first_with_best_per_version() {
        let mut s = MemoryStore::default();
        let a = add_version(&mut s, "{ 1 }");
        let b = add_version(&mut s, "{ 2 }");
        add_view(&mut s, &a, 1, &[("test::t", true, "")], Some(500));
        add_view(
            &mut s,
            &a,
            2,
            &[("test::t", true, ""), ("test::u", true, "")],
            Some(300),
        );
        add_view(&mut s, &b, 3, &[("test::t", true, "")], Some(400));
        let got = passing_by_bench(&s, &id("f"), &id("bench::f")).unwrap();
        assert_eq!(got, vec![(a, 300), (b, 400)]);
    }

    #[test]
    fn ties_break_by_version_id() {
        let mut s = MemoryStore::default();
        let a = add_version(&mut s, "{ 1 }");
        let b = add_version(&mut s, "{ 2 }");
        add_view(&mut s, &a, 1, &[("test::t", true, "")], Some(100));
        add_view(&mut s, &b, 2, &[("test::t", true, "")], Some(100));
        let got = passing_by_bench(&s, &id("f"), &id("bench::f")).unwrap();
        let mut expect = vec![(a, 100), (b, 100)];
        expect.sort();
        assert_eq!(got, expect);
    }

    #[test]
    fn failing_views_and_missing_bench_are_excluded() {
        let mut s = MemoryStore::default();
        let a = add_version(&mut s, "{ 1 }");
        let b = add_version(&mut s, "{ 2 }");
        add_view(&mut s, &a, 1, &[("test::t", false, "boom")], Some(10));
        add_view(&mut s, &b, 2, &[("test::t", true, "")], None);
        assert!(passing_by_bench(&s, &id("f"), &id("bench::f"))
            .unwrap()
            .is_empty());
        assert!(passing_by_bench(&s, &id("f"), &id("bench::other"))
            .unwrap()
            .is_empty());
    }

    #[test]
    fn empty_store_yields_nothing() {
        let s = MemoryStore::default();
        assert!(passing_by_bench(&s, &id("f"), &id("bench::f"))
            .unwrap()
            .is_empty());
        assert!(failures_for(&s, &id("f")).unwrap().is_empty());
    }

    #[test]
    fn failures_include_text_oldest_first_deduplicated() {
        let mut s = MemoryStore::default();
        let a = add_version(&mut s, "{ 1 }");
        let b = add_version(&mut s, "{ 2 }");
        let c = add_version(&mut s, "{ 3 }");
        add_view(
            &mut s,
            &a,
            1,
            &[("test::t", false, "left 1 right 2"), ("test::u", true, "")],
            None,
        );
        add_view(
            &mut s,
            &b,
            2,
            &[
                ("test::t", false, "left 1 right 2"),
                ("test::v", false, "panicked"),
            ],
            None,
        );
        add_view(&mut s, &c, 3, &[("test::t", false, "latest failure")], None);
        let got = failures_for(&s, &id("f")).unwrap();
        assert_eq!(
            got,
            vec!["left 1 right 2".to_string(), "panicked".to_string()]
        );
    }

    #[test]
    fn failures_only_for_tests_reaching_node() {
        let mut s = MemoryStore::default();
        let a = add_version(&mut s, "{ 1 }");
        add_version(&mut s, "{ 2 }");
        let view = View::new(
            RunId("r".into()),
            None,
            BTreeMap::from([(id("f"), a)]),
            BTreeMap::from([(id("test::other"), BTreeSet::from([id("g")]))]),
            1,
        );
        let view_id = view.id.clone();
        s.put_view(view).unwrap();
        s.put_results(Results {
            view: view_id,
            tests: vec![TestOutcome {
                test: id("test::other"),
                passed: false,
                message: "unrelated".into(),
            }],
            benches: vec![],
            clippy_clean: None,
            recorded_ms: 1,
        })
        .unwrap();
        assert!(failures_for(&s, &id("f")).unwrap().is_empty());
    }
}
