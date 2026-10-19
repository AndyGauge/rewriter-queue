use crate::fill::{build_prompt, extract_body, Generator, Outcome, Recorder};
use crate::model::{Graph, NodeId, NodeKind, TestOutcome};
use crate::version::{RunId, Store, StoreError, View};
use std::collections::BTreeMap;

fn generate_round(
    graph: &Graph,
    generators: &mut [Box<dyn Generator + Send>],
    todo: &[(NodeId, Vec<String>)],
) -> Vec<Result<String, String>> {
    let width = generators.len();
    let mut results: Vec<Option<Result<String, String>>> = vec![None; todo.len()];
    std::thread::scope(|scope| {
        let handles: Vec<_> = generators
            .iter_mut()
            .enumerate()
            .map(|(g, generator)| {
                scope.spawn(move || {
                    (g..todo.len())
                        .step_by(width)
                        .map(|i| {
                            let (id, errors) = &todo[i];
                            let prompt = build_prompt(graph, id, errors);
                            let attempt = generator.generate(&prompt).and_then(|r| {
                                extract_body(&r)
                                    .ok_or_else(|| "no `{ ... }` block in response".to_string())
                            });
                            (i, attempt)
                        })
                        .collect::<Vec<_>>()
                })
            })
            .collect();
        for handle in handles {
            for (i, attempt) in handle.join().expect("generator thread panicked") {
                results[i] = Some(attempt);
            }
        }
    });
    results
        .into_iter()
        .map(|r| r.unwrap_or_else(|| Err("no result".to_string())))
        .collect()
}

/// Fill each dependency level (`order::levels`) concurrently, one worker per
/// generator, then check the level before moving on. Results are merged in
/// `NodeId` order so output matches a serial run.
pub fn fill_parallel(
    graph: &mut Graph,
    generators: &mut [Box<dyn Generator + Send>],
    check: &(dyn Fn(&Graph) -> BTreeMap<NodeId, Vec<String>> + Sync),
    max_retries: usize,
) -> Outcome {
    match fill_parallel_impl(graph, generators, check, max_retries, None) {
        Ok(outcome) => outcome,
        Err(_) => unreachable!("no store without a recorder"),
    }
}

/// `fill_parallel` that stores every candidate body as a version (parent is the
/// node's previous attempt), records one view and results per round, and adds
/// earlier failures for each node to its retry prompt. Returns the final view.
#[allow(clippy::too_many_arguments)]
pub fn fill_parallel_versioned(
    graph: &mut Graph,
    generators: &mut [Box<dyn Generator + Send>],
    check: &(dyn Fn(&Graph) -> BTreeMap<NodeId, Vec<String>> + Sync),
    max_retries: usize,
    store: &mut dyn Store,
    run: &RunId,
    model: &str,
    parent: Option<&View>,
) -> Result<(Outcome, View), StoreError> {
    let mut rec = Recorder::new(store, run, model, parent);
    let outcome = fill_parallel_impl(graph, generators, check, max_retries, Some(&mut rec))?;
    let view = rec.finish(graph)?;
    Ok((outcome, view))
}

fn fill_parallel_impl(
    graph: &mut Graph,
    generators: &mut [Box<dyn Generator + Send>],
    check: &(dyn Fn(&Graph) -> BTreeMap<NodeId, Vec<String>> + Sync),
    max_retries: usize,
    mut rec: Option<&mut Recorder<'_>>,
) -> Result<Outcome, StoreError> {
    let mut outcome = Outcome {
        filled: Vec::new(),
        failed: Vec::new(),
        retries: BTreeMap::new(),
        rollbacks: BTreeMap::new(),
    };
    let levels = match crate::order::levels(graph) {
        Ok(levels) => levels,
        Err(_) => vec![graph.nodes.keys().cloned().collect()],
    };

    for level in levels {
        let mut pending: Vec<(NodeId, Vec<String>)> = level
            .into_iter()
            .filter(|id| {
                graph
                    .nodes
                    .get(id)
                    .is_some_and(|n| !n.is_filled() && n.kind != NodeKind::Test)
                    && !graph.exhausted(id)
            })
            .map(|id| (id, Vec::new()))
            .collect();
        let mut done: Vec<NodeId> = Vec::new();
        let mut attempts: BTreeMap<NodeId, usize> = BTreeMap::new();

        for round in 0..=max_retries {
            if pending.is_empty() {
                break;
            }
            if let Some(r) = rec.as_mut() {
                for (id, errs) in pending.iter_mut() {
                    *errs = r.prompt_errors(id, errs)?;
                }
            }
            let results = if generators.is_empty() {
                vec![Err("no generators".to_string()); pending.len()]
            } else {
                generate_round(graph, generators, &pending)
            };
            let mut generated = false;
            for ((id, _), result) in pending.iter().zip(&results) {
                if let Ok(body) = result {
                    if let Some(node) = graph.nodes.get_mut(id) {
                        node.body = Some(body.clone());
                        generated = true;
                        graph.spend(id);
                    }
                }
            }
            let mut errors = if generated {
                check(graph)
            } else {
                BTreeMap::new()
            };
            if let Some(r) = rec.as_mut() {
                let mut tests: Vec<TestOutcome> = Vec::new();
                for ((id, shown), result) in pending.iter().zip(&results) {
                    if result.is_ok() {
                        r.attempt(graph, id, &build_prompt(graph, id, shown))?;
                        for e in errors.get(id).into_iter().flatten() {
                            tests.push(TestOutcome {
                                test: id.clone(),
                                passed: false,
                                message: e.clone(),
                            });
                        }
                    }
                }
                r.commit(graph, tests)?;
                for ((id, _), result) in pending.iter().zip(&results) {
                    let errs = match result {
                        Err(e) => vec![e.clone()],
                        Ok(_) => errors.get(id).cloned().unwrap_or_default(),
                    };
                    r.note_failures(id, &errs);
                }
            }
            let mut next = Vec::new();
            for ((id, _), result) in pending.into_iter().zip(results) {
                if round > 0 {
                    attempts.insert(id.clone(), round);
                }
                let errs = match result {
                    Err(e) => vec![e],
                    Ok(_) => errors.remove(&id).unwrap_or_default(),
                };
                if errs.is_empty() {
                    done.push(id);
                } else if round == max_retries || graph.exhausted(&id) {
                    if let Some(node) = graph.nodes.get_mut(&id) {
                        node.body = None;
                    }
                    outcome.failed.push((id, errs.join("\n")));
                } else {
                    next.push((id, errs));
                }
            }
            pending = next;
        }
        done.sort();
        outcome.filled.extend(done);
        outcome.retries.extend(attempts);
    }
    outcome.failed.sort();
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Node, Placement};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    fn func(name: &str) -> Node {
        Node {
            id: NodeId(name.to_string()),
            kind: NodeKind::Function,
            signature: format!("fn {name}()"),
            body: None,
            impl_header: None,
            placement: Placement {
                file: "lib.rs".into(),
                index: 0,
            },
        }
    }

    fn sample() -> Graph {
        let mut g = Graph::default();
        for n in ["a", "b", "c", "d", "e", "top"] {
            g.insert(func(n));
        }
        for n in ["a", "b", "c", "d"] {
            g.add_use(&NodeId("top".into()), &NodeId(n.into()));
        }
        g.add_use(&NodeId("e".into()), &NodeId("a".into()));
        g
    }

    struct Fake {
        delay: Duration,
        log: Option<Arc<Mutex<Vec<String>>>>,
        calls: Arc<AtomicUsize>,
    }

    impl Fake {
        fn plain() -> Self {
            Fake {
                delay: Duration::ZERO,
                log: None,
                calls: Arc::new(AtomicUsize::new(0)),
            }
        }
    }

    impl Generator for Fake {
        fn generate(&mut self, prompt: &str) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(self.delay);
            if let Some(log) = &self.log {
                log.lock().unwrap().push(prompt.to_string());
            }
            let name = prompt
                .split("fn ")
                .nth(1)
                .and_then(|s| s.split('(').next())
                .unwrap_or("?");
            if name == "bad" || (name == "b" && !prompt.contains("previous attempt")) {
                return Ok("{ BROKEN }".to_string());
            }
            Ok(format!("{{ /* {name} */ }}"))
        }
    }

    fn check(g: &Graph) -> BTreeMap<NodeId, Vec<String>> {
        g.nodes
            .values()
            .filter(|n| n.body.as_deref() == Some("{ BROKEN }"))
            .map(|n| (n.id.clone(), vec!["broken".to_string()]))
            .collect()
    }

    #[test]
    fn matches_serial_run() {
        let mut serial = sample();
        let mut gen = Fake::plain();
        let s = crate::fill::fill_all(&mut serial, &mut gen, &mut |g| check(g), 2);

        let mut par = sample();
        let mut gens: Vec<Box<dyn Generator + Send>> = vec![
            Box::new(Fake::plain()),
            Box::new(Fake::plain()),
            Box::new(Fake::plain()),
        ];
        let p = fill_parallel(&mut par, &mut gens, &check, 2);

        assert_eq!(serial, par);
        let mut sf = s.filled.clone();
        sf.sort();
        let mut pf = p.filled.clone();
        pf.sort();
        assert_eq!(sf, pf);
        assert_eq!(p.filled.last(), Some(&NodeId("top".into())));
        assert!(p.failed.is_empty());
    }

    #[test]
    fn levels_run_concurrently() {
        let mut g = Graph::default();
        for n in ["a", "b", "c", "d"] {
            g.insert(func(n));
        }
        let mut gens: Vec<Box<dyn Generator + Send>> = (0..4)
            .map(|_| {
                Box::new(Fake {
                    delay: Duration::from_millis(300),
                    ..Fake::plain()
                }) as Box<dyn Generator + Send>
            })
            .collect();
        let start = Instant::now();
        let out = fill_parallel(&mut g, &mut gens, &|_| BTreeMap::new(), 0);
        let elapsed = start.elapsed();
        assert_eq!(out.filled.len(), 4);
        assert!(elapsed < Duration::from_millis(900), "took {elapsed:?}");
    }

    #[test]
    fn retries_only_failing_node() {
        let mut g = sample();
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut gens: Vec<Box<dyn Generator + Send>> = vec![Box::new(Fake {
            log: Some(log.clone()),
            ..Fake::plain()
        })];
        let out = fill_parallel(&mut g, &mut gens, &check, 2);
        assert!(out.failed.is_empty());
        assert_eq!(out.retries.len(), 1);
        assert_eq!(out.retries[&NodeId("b".into())], 1);
        let log = log.lock().unwrap();
        let b_prompts: Vec<_> = log
            .iter()
            .filter(|p| p.contains("\n\n```rust\nfn b()"))
            .collect();
        assert_eq!(b_prompts.len(), 2);
        assert!(b_prompts[1].contains("broken"));
        assert_eq!(
            log.iter()
                .filter(|p| p.contains("\n\n```rust\nfn a()"))
                .count(),
            1
        );
    }

    #[test]
    fn persistent_failure_is_reset() {
        let mut g = Graph::default();
        g.insert(func("bad"));
        g.insert(func("ok"));
        let mut gens: Vec<Box<dyn Generator + Send>> = vec![Box::new(Fake::plain())];
        let out = fill_parallel(&mut g, &mut gens, &check, 1);
        assert_eq!(out.failed.len(), 1);
        assert_eq!(out.failed[0].0, NodeId("bad".into()));
        assert!(g.nodes[&NodeId("bad".into())].body.is_none());
        assert_eq!(out.filled, vec![NodeId("ok".into())]);
    }

    #[test]
    fn versioned_keeps_both_attempts_and_prompts_with_failure() {
        use crate::version::MemoryStore;
        let mut g = sample();
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut gens: Vec<Box<dyn Generator + Send>> = vec![Box::new(Fake {
            log: Some(log.clone()),
            ..Fake::plain()
        })];
        let mut store = MemoryStore::default();
        let (out, view) = fill_parallel_versioned(
            &mut g,
            &mut gens,
            &check,
            2,
            &mut store,
            &RunId("r".into()),
            "m",
            None,
        )
        .unwrap();
        assert!(out.failed.is_empty());
        let b = NodeId("b".into());
        let versions = store.versions_of(&b).unwrap();
        assert_eq!(versions.len(), 2);
        let first = store.get_version(&versions[0]).unwrap();
        let second = store.get_version(&versions[1]).unwrap();
        assert_eq!(first.body.as_deref(), Some("{ BROKEN }"));
        assert_eq!(second.parent, Some(first.id));
        assert_eq!(view.versions[&b], second.id);
        assert!(store
            .all_results()
            .unwrap()
            .iter()
            .any(|r| r.tests.iter().any(|t| !t.passed && t.message == "broken")));
        let log = log.lock().unwrap();
        assert!(log
            .iter()
            .any(|p| p.contains("fn b()") && p.contains("broken")));
    }
}
