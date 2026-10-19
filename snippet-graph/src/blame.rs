use crate::model::{Graph, NodeId, NodeKind, TestOutcome};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq)]
pub struct Suspect {
    pub node: NodeId,
    pub score: f64,
}

/// A test that fails by reaching a `todo!()` says a dependency is unfilled, not that
/// anything it touches is wrong; unfilled nodes are refilled by the fill pass itself.
const UNFILLED_PANIC: &str = "not yet implemented";
const RECENT_BONUS: f64 = 0.5;
const PASSING_PENALTY: f64 = 1.0;

/// Rank function and method nodes (the only ones with a body to refill) by how likely they are to be the cause of the failing
/// outcomes, highest score first, ties broken by `NodeId`. A node reached by a
/// passing test is down-weighted; a node reached by few failing tests, or only
/// by failing tests, and any node in `recently_filled`, is up-weighted. Nodes
/// not reached by any failing test never appear. A suspect that depends on
/// another suspect ranks after it, so the deepest failing helper is retried first.
pub fn rank(graph: &Graph, outcomes: &[TestOutcome], recently_filled: &[NodeId]) -> Vec<Suspect> {
    let mut reach: BTreeMap<&NodeId, (usize, usize)> = BTreeMap::new();
    for outcome in outcomes {
        if !outcome.passed && outcome.message.contains(UNFILLED_PANIC) {
            continue;
        }
        for subject in graph.subjects_of(&outcome.test) {
            let fillable = graph
                .nodes
                .get(subject)
                .is_some_and(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method));
            if !fillable || graph.frozen.contains(subject) {
                continue;
            }
            let entry = reach.entry(subject).or_default();
            if outcome.passed {
                entry.0 += 1;
            } else {
                entry.1 += 1;
            }
        }
    }

    let mut suspects: Vec<Suspect> = reach
        .into_iter()
        .filter(|(_, (_, failing))| *failing > 0)
        .map(|(node, (passing, failing))| {
            let total = (passing + failing) as f64;
            let mut score = failing as f64 / total + 1.0 / total;
            if passing > 0 {
                score -= PASSING_PENALTY;
            }
            if recently_filled.contains(node) {
                score += RECENT_BONUS;
            }
            Suspect {
                node: node.clone(),
                score,
            }
        })
        .collect();

    suspects.sort_by(|a, b| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| a.node.cmp(&b.node))
    });
    let suspect_ids: std::collections::BTreeSet<NodeId> =
        suspects.iter().map(|s| s.node.clone()).collect();
    let (leaves, callers): (Vec<Suspect>, Vec<Suspect>) = suspects.into_iter().partition(|s| {
        !graph
            .dependencies(&s.node)
            .any(|dep| suspect_ids.contains(dep))
    });
    leaves.into_iter().chain(callers).collect()
}

/// The suspects to retry together in one repair round: those that do not depend
/// on another suspect, in rank order, at most `limit`. Falls back to the top
/// suspect when every suspect depends on another (a cycle). Nodes whose attempt
/// budget is spent are never chosen.
pub fn repair_targets(graph: &Graph, suspects: &[Suspect], limit: usize) -> Vec<NodeId> {
    let live: Vec<Suspect> = suspects
        .iter()
        .filter(|s| !graph.exhausted(&s.node))
        .cloned()
        .collect();
    let suspects = live.as_slice();
    let ids: std::collections::BTreeSet<&NodeId> = suspects.iter().map(|s| &s.node).collect();
    let mut targets: Vec<NodeId> = suspects
        .iter()
        .filter(|s| !graph.dependencies(&s.node).any(|dep| ids.contains(dep)))
        .take(limit)
        .map(|s| s.node.clone())
        .collect();
    if targets.is_empty() {
        targets.extend(suspects.first().map(|s| s.node.clone()));
    }
    targets
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Node, NodeKind, Placement};

    fn id(s: &str) -> NodeId {
        NodeId(s.to_string())
    }

    fn graph(edges: &[(&str, &[&str])]) -> Graph {
        let mut g = Graph::default();
        for (test, subjects) in edges {
            let mut add = |name: &str, kind: NodeKind| {
                g.insert(Node {
                    id: id(name),
                    kind,
                    signature: String::new(),
                    body: None,
                    impl_header: None,
                    placement: Placement {
                        file: "lib.rs".into(),
                        index: 0,
                    },
                });
            };
            add(test, NodeKind::Test);
            for s in *subjects {
                add(s, NodeKind::Function);
            }
        }
        for (test, subjects) in edges {
            for s in *subjects {
                g.add_use(&id(test), &id(s));
            }
        }
        g
    }

    fn out(test: &str, passed: bool) -> TestOutcome {
        TestOutcome {
            test: id(test),
            passed,
            message: String::new(),
        }
    }

    fn names(s: &[Suspect]) -> Vec<&str> {
        s.iter().map(|s| s.node.0.as_str()).collect()
    }

    struct Case {
        name: &'static str,
        edges: Vec<(&'static str, Vec<&'static str>)>,
        outcomes: Vec<(&'static str, bool)>,
        recent: Vec<&'static str>,
        expected: Vec<&'static str>,
    }

    #[test]
    fn blame_table() {
        let cases = vec![
            Case {
                name: "single test blames its subject",
                edges: vec![("t1", vec!["a"])],
                outcomes: vec![("t1", false)],
                recent: vec![],
                expected: vec!["a"],
            },
            Case {
                name: "shared subject cleared by passing test",
                edges: vec![("t1", vec!["a", "b"]), ("t2", vec!["b"])],
                outcomes: vec![("t1", false), ("t2", true)],
                recent: vec![],
                expected: vec!["a", "b"],
            },
            Case {
                name: "tie broken by node id",
                edges: vec![("t1", vec!["z", "m", "a"])],
                outcomes: vec![("t1", false)],
                recent: vec![],
                expected: vec!["a", "m", "z"],
            },
            Case {
                name: "recently filled bump",
                edges: vec![("t1", vec!["a", "b"])],
                outcomes: vec![("t1", false)],
                recent: vec!["b"],
                expected: vec!["b", "a"],
            },
            Case {
                name: "no failing tests",
                edges: vec![("t1", vec!["a"])],
                outcomes: vec![("t1", true)],
                recent: vec!["a"],
                expected: vec![],
            },
            Case {
                name: "fewer reaching tests ranks higher",
                edges: vec![("t1", vec!["a", "b"]), ("t2", vec!["b"])],
                outcomes: vec![("t1", false), ("t2", false)],
                recent: vec![],
                expected: vec!["a", "b"],
            },
        ];
        for case in cases {
            let g = graph(
                &case
                    .edges
                    .iter()
                    .map(|(t, s)| (*t, s.as_slice()))
                    .collect::<Vec<_>>(),
            );
            let outcomes: Vec<_> = case.outcomes.iter().map(|(t, p)| out(t, *p)).collect();
            let recent: Vec<_> = case.recent.iter().map(|s| id(s)).collect();
            let ranked = rank(&g, &outcomes, &recent);
            assert_eq!(names(&ranked), case.expected, "{}", case.name);
            assert_eq!(ranked, rank(&g, &outcomes, &recent), "{}", case.name);
        }
    }

    #[test]
    fn non_function_subjects_are_never_suspects() {
        let mut g = graph(&[("t", &["f", "Err"])]);
        g.nodes.get_mut(&id("Err")).unwrap().kind = NodeKind::Type;
        let ranked = rank(&g, &[out("t", false)], &[]);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].node, id("f"));
    }

    #[test]
    fn deepest_suspect_ranks_first() {
        let mut g = graph(&[("t_caller", &["caller"]), ("t_helper", &["helper"])]);
        g.add_use(&id("caller"), &id("helper"));
        let ranked = rank(&g, &[out("t_caller", false), out("t_helper", false)], &[]);
        assert_eq!(ranked[0].node, id("helper"));
        assert_eq!(ranked[1].node, id("caller"));
    }

    #[test]
    fn failures_that_hit_an_unfilled_todo_blame_no_one() {
        let g = graph(&[("t_top", &["top", "helper"])]);
        let todo = TestOutcome {
            test: id("t_top"),
            passed: false,
            message: "thread 't' panicked at src/lib.rs:9:5:\nnot yet implemented".into(),
        };
        assert!(rank(&g, &[todo], &[]).is_empty());
        let wrong = TestOutcome {
            test: id("t_top"),
            passed: false,
            message: "assertion failed".into(),
        };
        assert_eq!(rank(&g, &[wrong], &[]).len(), 2);
    }

    #[test]
    fn repair_targets_are_all_independent_leaves_up_to_limit() {
        let mut g = graph(&[
            ("t1", &["a"]),
            ("t2", &["b"]),
            ("t3", &["c"]),
            ("t4", &["d"]),
        ]);
        g.add_use(&id("d"), &id("a"));
        let outcomes: Vec<TestOutcome> = ["t1", "t2", "t3", "t4"]
            .iter()
            .map(|t| out(t, false))
            .collect();
        let ranked = rank(&g, &outcomes, &[]);
        let all = repair_targets(&g, &ranked, 10);
        assert_eq!(all.len(), 3);
        assert!(!all.contains(&id("d")));
        assert_eq!(repair_targets(&g, &ranked, 2).len(), 2);
    }

    #[test]
    fn repair_targets_fall_back_to_top_suspect_in_a_cycle() {
        let mut g = graph(&[("t1", &["a"]), ("t2", &["b"])]);
        g.add_use(&id("a"), &id("b"));
        g.add_use(&id("b"), &id("a"));
        let ranked = rank(&g, &[out("t1", false), out("t2", false)], &[]);
        assert_eq!(repair_targets(&g, &ranked, 5).len(), 1);
    }
}
