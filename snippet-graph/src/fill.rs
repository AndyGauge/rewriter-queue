use crate::model::{Graph, Node, NodeId, NodeKind, TestOutcome, TestReport};
use crate::version::{
    content_hash, NodeVersion, Origin, Results, RunId, Store, StoreError, VersionId, View,
};
use std::collections::{BTreeMap, BTreeSet};

pub trait Generator {
    fn generate(&mut self, prompt: &str) -> Result<String, String>;
}

/// A repeated answer costs nothing, but an unbroken run of them this long is a
/// stuck model, and the next repeat counts as a retry.
const MAX_CONSECUTIVE_REPEATS: usize = 20;
const REPEAT_NOTE: &str =
    "That exact body was already tried and failed this way. Write a different implementation.";

pub struct Outcome {
    pub filled: Vec<NodeId>,
    pub failed: Vec<(NodeId, String)>,
    pub retries: BTreeMap<NodeId, usize>,
    /// Attempts that repeated an earlier answer for the same node. They reuse
    /// the earlier result and cost neither a retry nor budget.
    pub rollbacks: BTreeMap<NodeId, usize>,
}

/// Prompt for one node: its own signature plus the signatures (never bodies) of
/// the nodes it uses, and any prior compiler errors for this node.
pub fn build_prompt(graph: &Graph, id: &NodeId, prior_errors: &[String]) -> String {
    let mut out = String::new();
    if let Some(node) = graph.nodes.get(id) {
        out.push_str("Implement the body of this Rust item. Respond with only the body as a single `{ ... }` block.\n\n");
        if !graph.spec.is_empty() {
            out.push_str("Specification of the whole program. Follow it exactly, including any stated algorithm, formula, ordering or numeric method, even where you would do it differently:\n```\n");
            out.push_str(&graph.spec);
            out.push_str("\n```\n\nThe item to implement:\n");
        }
        if let Some(header) = &node.impl_header {
            out.push_str(&format!("Inside `{header}`:\n"));
        }
        out.push_str(&format!("```rust\n{}\n```\n", node.signature));

        let deps: Vec<&Node> = graph
            .dependencies(id)
            .filter_map(|d| graph.nodes.get(d))
            .collect();
        if !deps.is_empty() {
            out.push_str("\nAvailable items it may use (signatures and doc comments only):\n```rust\n");
            for dep in deps {
                if let Some(header) = &dep.impl_header {
                    out.push_str(&format!("// in {header}\n"));
                }
                out.push_str(&dep.signature);
                out.push('\n');
            }
            out.push_str("```\n");
        }
    }
    out.push_str(&format!(
        "\nIf this function needs several distinct steps (filtering, grouping, summing and so on), implement it anyway but make the first line of the body `{}: <the steps that should be separate functions>`.\n",
        crate::structure::MARKER_PREFIX
    ));
    let reason = graph.refactor.get(id).cloned().or_else(|| {
        crate::structure::test_load(graph, id).map(|f| f.detail)
    });
    if let Some(reason) = reason {
        out.push_str(&format!(
            "\nThis function was judged too large ({reason}). Do not write one long body. Put each distinct step in its own small nested `fn` inside the body, and keep the outer body a short sequence of calls to them.\n"
        ));
    }
    if !prior_errors.is_empty() {
        out.push_str("\nYour previous attempt failed with these errors; fix them:\n");
        for e in prior_errors {
            out.push_str(&format!("- {e}\n"));
        }
    }
    out
}

fn unfenced(response: &str) -> &str {
    let Some(start) = response.find("```") else {
        return response;
    };
    let after = &response[start + 3..];
    let content_start = after.find('\n').map_or(0, |i| i + 1);
    let content = &after[content_start..];
    match content.find("```") {
        Some(end) => &content[..end],
        None => content,
    }
}

/// Extract a single `{ ... }` block from model output, stripping markdown fences.
pub fn extract_body(response: &str) -> Option<String> {
    let text = unfenced(response);
    let chars: Vec<char> = text.chars().collect();
    let open = chars.iter().position(|&c| c == '{')?;
    let mut depth = 0usize;
    let mut i = open;
    while i < chars.len() {
        match chars[i] {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(chars[open..=i].iter().collect());
                }
            }
            '"' => i = skip_string(&chars, i),
            'r' if is_raw_start(&chars, i) => i = skip_raw_string(&chars, i),
            '\'' => i = skip_char_literal(&chars, i),
            '/' if chars.get(i + 1) == Some(&'/') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'*') => i = skip_block_comment(&chars, i),
            _ => {}
        }
        i += 1;
    }
    None
}

fn skip_string(chars: &[char], start: usize) -> usize {
    let mut i = start + 1;
    while i < chars.len() {
        match chars[i] {
            '\\' => i += 1,
            '"' => return i,
            _ => {}
        }
        i += 1;
    }
    chars.len()
}

fn is_raw_start(chars: &[char], i: usize) -> bool {
    if i > 0 && (chars[i - 1].is_alphanumeric() || chars[i - 1] == '_') {
        return false;
    }
    let mut j = i + 1;
    while chars.get(j) == Some(&'#') {
        j += 1;
    }
    chars.get(j) == Some(&'"')
}

fn skip_raw_string(chars: &[char], start: usize) -> usize {
    let mut j = start + 1;
    let mut hashes = 0;
    while chars[j] == '#' {
        hashes += 1;
        j += 1;
    }
    let mut i = j + 1;
    while i < chars.len() {
        if chars[i] == '"' && (0..hashes).all(|k| chars.get(i + 1 + k) == Some(&'#')) {
            return i + hashes;
        }
        i += 1;
    }
    chars.len()
}

fn skip_char_literal(chars: &[char], start: usize) -> usize {
    if chars.get(start + 1) == Some(&'\\') {
        let mut i = start + 3;
        while i < chars.len() && chars[i] != '\'' {
            i += 1;
        }
        return i;
    }
    if chars.get(start + 2) == Some(&'\'') {
        return start + 2;
    }
    start
}

fn skip_block_comment(chars: &[char], start: usize) -> usize {
    let mut depth = 0;
    let mut i = start;
    while i + 1 < chars.len() {
        if chars[i] == '/' && chars[i + 1] == '*' {
            depth += 1;
            i += 2;
        } else if chars[i] == '*' && chars[i + 1] == '/' {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return i - 1;
            }
        } else {
            i += 1;
        }
    }
    chars.len()
}

/// Fill unfilled nodes in dependency order, one at a time. After each fill, call
/// `check` with the current graph; if it reports errors attributed to that node,
/// retry up to `max_retries` times with the errors in the prompt, then mark the
/// node failed and continue.
pub fn fill_all(
    graph: &mut Graph,
    generator: &mut dyn Generator,
    check: &mut dyn FnMut(&Graph) -> BTreeMap<NodeId, Vec<String>>,
    max_retries: usize,
) -> Outcome {
    let mut outcome = Outcome {
        filled: Vec::new(),
        failed: Vec::new(),
        retries: BTreeMap::new(),
        rollbacks: BTreeMap::new(),
    };
    let order = match crate::order::topological(graph) {
        Ok(order) => order,
        Err(_) => graph.nodes.keys().cloned().collect(),
    };

    for id in order {
        match graph.nodes.get(&id) {
            Some(node) if !node.is_filled() => {}
            _ => continue,
        }
        let mut errors: Vec<String> = Vec::new();
        let mut succeeded = false;
        for _ in 0..=max_retries {
            let prompt = build_prompt(graph, &id, &errors);
            let attempt = generator.generate(&prompt).and_then(|r| {
                extract_body(&r).ok_or_else(|| "no `{ ... }` block in response".to_string())
            });
            match attempt {
                Err(e) => errors = vec![e],
                Ok(body) => {
                    graph.spend(&id);
                    if let Some(node) = graph.nodes.get_mut(&id) {
                        node.body = Some(body);
                    }
                    errors = check(graph).remove(&id).unwrap_or_default();
                    if errors.is_empty() {
                        succeeded = true;
                        break;
                    }
                }
            }
        }
        if succeeded {
            outcome.filled.push(id);
        } else {
            if let Some(node) = graph.nodes.get_mut(&id) {
                node.body = None;
            }
            outcome.failed.push((id, errors.join("\n")));
        }
    }
    outcome
}

fn affected_tests(graph: &Graph, ids: &[&NodeId]) -> Vec<NodeId> {
    let mut set: BTreeSet<NodeId> = BTreeSet::new();
    for id in ids {
        let mut reach = crate::order::dependents(graph, id);
        reach.push((*id).clone());
        for r in reach {
            if graph
                .nodes
                .get(&r)
                .is_some_and(|n| n.kind == NodeKind::Test)
            {
                set.insert(r.clone());
            }
            set.extend(graph.tests_for(&r).into_iter().cloned());
        }
    }
    set.into_iter().collect()
}

fn failure_texts(report: &TestReport) -> Vec<String> {
    let mut out: Vec<String> = report.unattributed.clone();
    for o in report.outcomes.iter().filter(|o| !o.passed) {
        out.push(format!("test {} failed: {}", o.test.0, o.message));
    }
    out
}

/// Fill unfilled non-test nodes in dependency order. After each fill, `run` is
/// called with only the tests affected by that node (tests reaching it or its
/// dependents). A build error or failing affected test triggers a retry of the
/// node `blame::rank` picks, with the failure text in the prompt, up to
/// `max_retries` per node; then that node is reset and marked failed.
pub fn fill_all_tested(
    graph: &mut Graph,
    generator: &mut dyn Generator,
    run: &mut dyn FnMut(&Graph, &[NodeId]) -> TestReport,
    max_retries: usize,
) -> Outcome {
    match fill_tested_impl(graph, generator, run, max_retries, None) {
        Ok(outcome) => outcome,
        Err(_) => unreachable!("no store without a recorder"),
    }
}

/// `fill_all_tested` that keeps every candidate body as a stored version (parent
/// is the node's previous attempt), records a view and results after each
/// attempt, and feeds earlier failures for the node into its retry prompt.
/// Returns the final view alongside the outcome.
#[allow(clippy::too_many_arguments)]
pub fn fill_all_versioned(
    graph: &mut Graph,
    generator: &mut dyn Generator,
    run: &mut dyn FnMut(&Graph, &[NodeId]) -> TestReport,
    max_retries: usize,
    store: &mut dyn Store,
    run_id: &RunId,
    model: &str,
    parent: Option<&View>,
) -> Result<(Outcome, View), StoreError> {
    let mut rec = Recorder::new(store, run_id, model, parent);
    let outcome = fill_tested_impl(graph, generator, run, max_retries, Some(&mut rec))?;
    let view = rec.finish(graph)?;
    Ok((outcome, view))
}

pub(crate) struct Recorder<'a> {
    store: &'a mut dyn Store,
    run: RunId,
    model: String,
    view: Option<View>,
    clock: u64,
    last: BTreeMap<NodeId, VersionId>,
    history: BTreeMap<NodeId, Vec<String>>,
}

impl<'a> Recorder<'a> {
    pub(crate) fn new(
        store: &'a mut dyn Store,
        run: &RunId,
        model: &str,
        parent: Option<&View>,
    ) -> Self {
        Recorder {
            store,
            run: run.clone(),
            model: model.to_string(),
            view: parent.cloned(),
            clock: parent.map_or(0, |p| p.created_ms + 1),
            last: parent.map(|p| p.versions.clone()).unwrap_or_default(),
            history: BTreeMap::new(),
        }
    }

    fn origin(&mut self, prompt: &str) -> Origin {
        self.clock += 1;
        Origin {
            run: self.run.clone(),
            model: self.model.clone(),
            prompt_hash: content_hash(&[prompt]),
            created_ms: self.clock,
        }
    }

    pub(crate) fn prompt_errors(
        &self,
        node: &NodeId,
        current: &[String],
    ) -> Result<Vec<String>, StoreError> {
        let mut out: Vec<String> = Vec::new();
        let earlier = crate::results::failures_for(&*self.store, node)?;
        let local = self.history.get(node).cloned().unwrap_or_default();
        for e in earlier
            .into_iter()
            .chain(local)
            .chain(current.iter().cloned())
        {
            if !out.contains(&e) {
                out.push(e);
            }
        }
        Ok(out)
    }

    pub(crate) fn note_failures(&mut self, node: &NodeId, errors: &[String]) {
        let h = self.history.entry(node.clone()).or_default();
        for e in errors {
            if !h.contains(e) {
                h.push(e.clone());
            }
        }
    }

    pub(crate) fn attempt(
        &mut self,
        graph: &Graph,
        node: &NodeId,
        prompt: &str,
    ) -> Result<(), StoreError> {
        let Some(n) = graph.nodes.get(node) else {
            return Ok(());
        };
        let origin = self.origin(prompt);
        let version = NodeVersion::new(n, self.last.get(node).cloned(), origin);
        let id = self.store.put_version(version)?;
        self.last.insert(node.clone(), id);
        Ok(())
    }

    pub(crate) fn commit(
        &mut self,
        graph: &Graph,
        tests: Vec<TestOutcome>,
    ) -> Result<(), StoreError> {
        let view = self.snapshot(graph)?;
        self.clock += 1;
        self.store.put_results(Results {
            view: view.id.clone(),
            tests,
            benches: Vec::new(),
            clippy_clean: None,
            recorded_ms: self.clock,
        })?;
        Ok(())
    }

    fn snapshot(&mut self, graph: &Graph) -> Result<View, StoreError> {
        let origin = self.origin("");
        let view =
            crate::view::from_graph(self.store, graph, &self.run, &origin, self.view.as_ref())?;
        self.store.put_view(view.clone())?;
        self.view = Some(view.clone());
        Ok(view)
    }

    pub(crate) fn finish(&mut self, graph: &Graph) -> Result<View, StoreError> {
        self.snapshot(graph)
    }
}

pub(crate) fn report_outcomes(report: &TestReport, target: &NodeId) -> Vec<TestOutcome> {
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
            test: target.clone(),
            passed: false,
            message: e.clone(),
        });
    }
    out
}

fn fill_tested_impl(
    graph: &mut Graph,
    generator: &mut dyn Generator,
    run: &mut dyn FnMut(&Graph, &[NodeId]) -> TestReport,
    max_retries: usize,
    mut rec: Option<&mut Recorder<'_>>,
) -> Result<Outcome, StoreError> {
    let mut outcome = Outcome {
        filled: Vec::new(),
        failed: Vec::new(),
        retries: BTreeMap::new(),
        rollbacks: BTreeMap::new(),
    };
    let order = match crate::order::topological(graph) {
        Ok(order) => order,
        Err(_) => graph.nodes.keys().cloned().collect(),
    };

    for id in order {
        match graph.nodes.get(&id) {
            Some(node) if !node.is_filled() && node.kind != NodeKind::Test => {}
            _ => continue,
        }
        if graph.exhausted(&id) {
            outcome
                .failed
                .push((id.clone(), "attempt budget spent".to_string()));
            continue;
        }
        let mut target = id.clone();
        let mut errors: Vec<String> = Vec::new();
        let mut seen: BTreeMap<(NodeId, String), (NodeId, Vec<String>)> = BTreeMap::new();
        let mut streak = 0;
        loop {
            let shown = match rec.as_mut() {
                Some(r) => r.prompt_errors(&target, &errors)?,
                None => errors.clone(),
            };
            let prompt = build_prompt(graph, &target, &shown);
            let attempt = generator.generate(&prompt).and_then(|r| {
                extract_body(&r).ok_or_else(|| "no `{ ... }` block in response".to_string())
            });
            let mut next: Option<(NodeId, Vec<String>)> = None;
            let attempted = target.clone();
            let mut attempted_body: Option<String> = None;
            let mut rolled_back = false;
            match attempt {
                Err(e) => next = Some((target.clone(), vec![e])),
                Ok(body) if seen.contains_key(&(target.clone(), body.clone())) => {
                    next = seen.get(&(target.clone(), body)).cloned();
                    if streak < MAX_CONSECUTIVE_REPEATS {
                        streak += 1;
                        *graph.rolled_back.entry(target.clone()).or_default() += 1;
                        *outcome.rollbacks.entry(target.clone()).or_default() += 1;
                        rolled_back = true;
                        if let Some((_, errs)) = next.as_mut() {
                            errs.push(REPEAT_NOTE.to_string());
                        }
                    }
                }
                Ok(body) => {
                    streak = 0;
                    attempted_body = Some(body.clone());
                    graph.spend(&target);
                    if let Some(node) = graph.nodes.get_mut(&target) {
                        node.body = Some(body);
                    }
                    let tests = affected_tests(graph, &[&id, &target]);
                    let report = run(graph, &tests);
                    if let Some(r) = rec.as_mut() {
                        r.attempt(graph, &target, &prompt)?;
                        r.commit(graph, report_outcomes(&report, &target))?;
                    }
                    let build_target = if report.build_errors.contains_key(&target) {
                        Some(target.clone())
                    } else {
                        report
                            .build_errors
                            .keys()
                            .find(|k| graph.nodes.get(*k).is_some_and(|n| n.is_filled()))
                            .cloned()
                    };
                    if let Some(t) = build_target {
                        let errs = report.build_errors[&t].clone();
                        next = Some((t, errs));
                    } else if !report.compiled && !report.unattributed.is_empty() {
                        next = Some((target.clone(), report.unattributed.clone()));
                    } else if report.outcomes.iter().any(|o| !o.passed) {
                        let mut recent = outcome.filled.clone();
                        recent.push(id.clone());
                        let suspect = crate::blame::rank(graph, &report.outcomes, &recent)
                            .into_iter()
                            .map(|s| s.node)
                            .find(|n| graph.nodes.get(n).is_some_and(|n| n.is_filled()))
                            .unwrap_or_else(|| target.clone());
                        next = Some((suspect, failure_texts(&report)));
                    }
                }
            }
            let Some((t, errs)) = next else {
                outcome.filled.push(id.clone());
                break;
            };
            if let Some(body) = attempted_body {
                seen.insert((attempted, body), (t.clone(), errs.clone()));
            }
            if let Some(r) = rec.as_mut() {
                r.note_failures(&t, &errs);
            }
            target = t;
            errors = errs;
            if rolled_back {
                continue;
            }
            let used = outcome.retries.get(&target).copied().unwrap_or(0);
            if used >= max_retries || graph.exhausted(&target) {
                if let Some(node) = graph.nodes.get_mut(&target) {
                    node.body = None;
                }
                outcome.filled.retain(|f| f != &target);
                outcome.failed.push((target, errors.join("\n")));
                break;
            }
            *outcome.retries.entry(target.clone()).or_default() += 1;
        }
    }
    Ok(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Placement, TestOutcome};
    use std::collections::VecDeque;

    struct Fake(VecDeque<Result<String, String>>, Vec<String>);

    impl Generator for Fake {
        fn generate(&mut self, prompt: &str) -> Result<String, String> {
            self.1.push(prompt.to_string());
            self.0
                .pop_front()
                .unwrap_or_else(|| Err("exhausted".into()))
        }
    }

    fn func(name: &str, sig: &str) -> Node {
        Node {
            id: NodeId(name.into()),
            kind: NodeKind::Function,
            signature: sig.into(),
            body: None,
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

    #[test]
    fn extract_strips_fences() {
        let r = "Here:\n```rust\n{ let x = 1; x }\n```\nDone";
        assert_eq!(extract_body(r).unwrap(), "{ let x = 1; x }");
    }

    #[test]
    fn extract_handles_nested_and_literals() {
        let r = "fn f() { if a { \"}\" } else { '}' } // }\n /* } */ r#\"}\"# }\ntrailing }";
        let b = extract_body(r).unwrap();
        assert!(b.ends_with("r#\"}\"# }"), "{b}");
    }

    #[test]
    fn extract_lifetime_is_not_char() {
        let b = extract_body("{ let x: &'a str = \"\"; }").unwrap();
        assert_eq!(b, "{ let x: &'a str = \"\"; }");
    }

    #[test]
    fn extract_unbalanced_is_none() {
        assert!(extract_body("{ oops").is_none());
        assert!(extract_body("no braces").is_none());
    }

    #[test]
    fn prompt_always_offers_the_marker_and_asks_for_a_restructure_when_flagged() {
        let mut g = Graph::default();
        g.insert(func("a", "fn a() -> u32"));
        let plain = build_prompt(&g, &nid("a"), &[]);
        assert!(plain.contains("// TODO: does too much:"));
        assert!(!plain.contains("judged too large"));
        g.refactor
            .insert(nid("a"), "complexity 12 (limit 8)".to_string());
        let flagged = build_prompt(&g, &nid("a"), &[]);
        assert!(flagged.contains("judged too large (complexity 12 (limit 8))"));
        assert!(flagged.contains("nested `fn`"));
    }

    #[test]
    fn prompt_asks_for_a_restructure_when_many_tests_reach_the_function() {
        let mut g = Graph::default();
        g.insert(func("a", "fn a() -> u32"));
        for i in 0..6 {
            let name = format!("a_case{i}");
            let mut t = func(&name, &format!("#[test] fn {name}()"));
            t.kind = NodeKind::Test;
            t.body = Some("{}".into());
            g.insert(t);
            g.add_use(&nid(&name), &nid("a"));
        }
        let p = build_prompt(&g, &nid("a"), &[]);
        assert!(p.contains("6 tests written for it (limit 5)"));
    }

    #[test]
    fn prompt_has_signatures_not_bodies() {
        let mut g = Graph::default();
        g.insert(func("a", "fn a() -> u32"));
        let mut b = func("b", "fn b() -> u32");
        b.body = Some("{ SECRET }".into());
        g.insert(b);
        g.add_use(&nid("a"), &nid("b"));
        let p = build_prompt(&g, &nid("a"), &["boom".into()]);
        assert!(p.contains("fn a() -> u32"));
        assert!(p.contains("fn b() -> u32"));
        assert!(!p.contains("SECRET"));
        assert!(p.contains("boom"));
    }

    #[test]
    fn fill_all_retries_then_fails_and_continues() {
        let mut g = Graph::default();
        g.insert(func("a", "fn a()"));
        g.insert(func("b", "fn b()"));
        let mut gen = Fake(
            VecDeque::from([
                Ok("{ bad }".to_string()),
                Ok("{ still bad }".to_string()),
                Ok("{ good }".to_string()),
            ]),
            Vec::new(),
        );
        let mut check = |g: &Graph| {
            let mut m = BTreeMap::new();
            for (id, n) in &g.nodes {
                if n.body.as_deref().is_some_and(|b| b.contains("bad")) {
                    m.insert(id.clone(), vec!["err".to_string()]);
                }
            }
            m
        };
        let out = fill_all(&mut g, &mut gen, &mut check, 1);
        assert_eq!(out.failed.len(), 1);
        assert_eq!(out.filled.len(), 1);
        let failed = &out.failed[0].0;
        assert!(g.nodes[failed].body.is_none());
        assert!(gen.1[1].contains("err"));
    }

    #[test]
    fn tested_fill_retries_only_wrong_node_and_skips_unaffected_tests() {
        let mut g = Graph::default();
        g.insert(func("a", "fn a()"));
        g.insert(func("b", "fn b()"));
        let mut ta = func("t_a", "#[test] fn t_a()");
        ta.kind = NodeKind::Test;
        ta.body = Some("{}".into());
        let mut tb = func("t_b", "#[test] fn t_b()");
        tb.kind = NodeKind::Test;
        tb.body = Some("{}".into());
        g.insert(ta);
        g.insert(tb);
        g.add_use(&nid("t_a"), &nid("a"));
        g.add_use(&nid("t_b"), &nid("b"));
        let mut gen = Fake(
            VecDeque::from([
                Ok("{ wrong }".to_string()),
                Ok("{ right }".to_string()),
                Ok("{ right }".to_string()),
            ]),
            Vec::new(),
        );
        let mut calls: Vec<Vec<NodeId>> = Vec::new();
        let mut run = |g: &Graph, tests: &[NodeId]| {
            calls.push(tests.to_vec());
            let outcomes = tests
                .iter()
                .map(|t| {
                    let subject = t.0.trim_start_matches("t_");
                    let ok = !g.nodes[&nid(subject)]
                        .body
                        .as_deref()
                        .unwrap_or("")
                        .contains("wrong");
                    TestOutcome {
                        test: t.clone(),
                        passed: ok,
                        message: "expected 1, got 2".into(),
                    }
                })
                .collect();
            TestReport {
                compiled: true,
                outcomes,
                ..Default::default()
            }
        };
        let out = fill_all_tested(&mut g, &mut gen, &mut run, 2);
        assert!(out.failed.is_empty());
        assert_eq!(out.filled, vec![nid("a"), nid("b")]);
        assert_eq!(out.retries.len(), 1);
        assert_eq!(out.retries[&nid("a")], 1);
        assert!(gen.1[1].contains("expected 1, got 2"));
        assert!(gen.1[1].contains("fn a()"));
        assert!(!gen.1[2].contains("expected 1, got 2"));
        assert_eq!(
            calls,
            vec![vec![nid("t_a")], vec![nid("t_a")], vec![nid("t_b")]]
        );
    }

    fn repeating_generator(answers: &[&str]) -> Fake {
        Fake(
            answers.iter().map(|a| Ok(format!("{{ {a} }}"))).collect(),
            Vec::new(),
        )
    }

    fn one_node_with_test() -> Graph {
        let mut g = Graph::default();
        g.insert(func("a", "fn a()"));
        let mut ta = func("t_a", "#[test] fn t_a()");
        ta.kind = NodeKind::Test;
        ta.body = Some("{}".into());
        g.insert(ta);
        g.add_use(&nid("t_a"), &nid("a"));
        g
    }

    fn run_fails_on_wrong(runs: &mut usize) -> impl FnMut(&Graph, &[NodeId]) -> TestReport + '_ {
        move |g: &Graph, tests: &[NodeId]| {
            *runs += 1;
            let wrong = g.nodes[&nid("a")]
                .body
                .as_deref()
                .unwrap_or("")
                .contains("wrong");
            TestReport {
                compiled: true,
                outcomes: tests
                    .iter()
                    .map(|t| TestOutcome {
                        test: t.clone(),
                        passed: !wrong,
                        message: "expected 1, got 2".into(),
                    })
                    .collect(),
                ..Default::default()
            }
        }
    }

    #[test]
    fn repeated_answer_is_a_rollback_not_a_retry() {
        let mut g = one_node_with_test();
        let mut gen = repeating_generator(&["wrong", "wrong", "wrong", "right"]);
        let mut runs = 0;
        let out = fill_all_tested(&mut g, &mut gen, &mut run_fails_on_wrong(&mut runs), 4);
        assert!(out.failed.is_empty());
        assert_eq!(out.retries[&nid("a")], 1);
        assert_eq!(out.rollbacks[&nid("a")], 2);
        assert_eq!(runs, 2);
    }

    #[test]
    fn repeats_are_free_and_tell_the_model_so() {
        let mut g = one_node_with_test();
        let mut gen = repeating_generator(&["wrong", "wrong", "wrong", "wrong", "wrong", "right"]);
        let mut runs = 0;
        let out = fill_all_tested(&mut g, &mut gen, &mut run_fails_on_wrong(&mut runs), 1);
        assert!(out.failed.is_empty());
        assert_eq!(out.rollbacks[&nid("a")], 4);
        assert_eq!(out.retries[&nid("a")], 1);
        assert!(gen.1[2].contains("already tried"));
        assert!(!gen.1[1].contains("already tried"));
    }

    #[test]
    fn a_stuck_model_is_stopped_after_a_long_run_of_repeats() {
        let mut g = one_node_with_test();
        let answers = vec!["wrong"; 60];
        let mut gen = repeating_generator(&answers);
        let mut runs = 0;
        let out = fill_all_tested(&mut g, &mut gen, &mut run_fails_on_wrong(&mut runs), 2);
        assert_eq!(out.rollbacks[&nid("a")], MAX_CONSECUTIVE_REPEATS);
        assert_eq!(out.failed.len(), 1);
        assert!(g.nodes[&nid("a")].body.is_none());
    }

    #[test]
    fn attempt_budget_is_spent_across_passes() {
        let mut g = one_node_with_test();
        g.budget = 3;
        let mut gen = repeating_generator(&["wrong 1", "wrong 2"]);
        let mut runs = 0;
        let first = fill_all_tested(&mut g, &mut gen, &mut run_fails_on_wrong(&mut runs), 1);
        assert_eq!(first.failed.len(), 1);
        assert_eq!(g.spent[&nid("a")], 2);
        let mut gen = repeating_generator(&["wrong 3", "right"]);
        let second = fill_all_tested(&mut g, &mut gen, &mut run_fails_on_wrong(&mut runs), 5);
        assert_eq!(g.spent[&nid("a")], 3);
        assert_eq!(second.failed.len(), 1);
        assert!(g.exhausted(&nid("a")));
        let mut gen = repeating_generator(&["right"]);
        let third = fill_all_tested(&mut g, &mut gen, &mut run_fails_on_wrong(&mut runs), 5);
        assert!(third.filled.is_empty());
        assert_eq!(third.failed.len(), 1);
        assert!(gen.1.is_empty());
    }

    #[test]
    fn tested_fill_gives_up_after_max_retries() {
        let mut g = Graph::default();
        g.insert(func("a", "fn a()"));
        let mut gen = Fake(
            VecDeque::from([Ok("{ x }".to_string()), Ok("{ y }".to_string())]),
            Vec::new(),
        );
        let mut run = |_: &Graph, _: &[NodeId]| TestReport {
            compiled: false,
            build_errors: BTreeMap::from([(nid("a"), vec!["E0308".to_string()])]),
            ..Default::default()
        };
        let out = fill_all_tested(&mut g, &mut gen, &mut run, 1);
        assert_eq!(out.failed.len(), 1);
        assert_eq!(out.retries[&nid("a")], 1);
        assert!(g.nodes[&nid("a")].body.is_none());
        assert!(gen.1[1].contains("E0308"));
    }

    fn wrong_check(g: &Graph, tests: &[NodeId], msg: &str) -> TestReport {
        let outcomes = tests
            .iter()
            .map(|t| {
                let subject = t.0.trim_start_matches("t_");
                let ok = !g.nodes[&nid(subject)]
                    .body
                    .as_deref()
                    .unwrap_or("")
                    .contains("wrong");
                TestOutcome {
                    test: t.clone(),
                    passed: ok,
                    message: msg.into(),
                }
            })
            .collect();
        TestReport {
            compiled: true,
            outcomes,
            ..Default::default()
        }
    }

    fn tested_graph() -> Graph {
        let mut g = Graph::default();
        g.insert(func("a", "fn a()"));
        let mut ta = func("t_a", "#[test] fn t_a()");
        ta.kind = NodeKind::Test;
        ta.body = Some("{}".into());
        g.insert(ta);
        g.add_use(&nid("t_a"), &nid("a"));
        g
    }

    #[test]
    fn versioned_fill_keeps_every_attempt_and_failure_text() {
        use crate::version::MemoryStore;
        let mut g = tested_graph();
        let mut gen = Fake(
            VecDeque::from([
                Ok("{ wrong }".to_string()),
                Ok("{ wrong2 wrong }".to_string()),
                Ok("{ right }".to_string()),
            ]),
            Vec::new(),
        );
        let mut n = 0;
        let mut run = |g: &Graph, tests: &[NodeId]| {
            n += 1;
            wrong_check(g, tests, &format!("failure number {n}"))
        };
        let mut store = MemoryStore::default();
        let (out, view) = fill_all_versioned(
            &mut g,
            &mut gen,
            &mut run,
            3,
            &mut store,
            &RunId("r".into()),
            "m",
            None,
        )
        .unwrap();
        assert!(out.failed.is_empty());
        let versions = store.versions_of(&nid("a")).unwrap();
        assert_eq!(versions.len(), 3);
        let first = store.get_version(&versions[0]).unwrap();
        let second = store.get_version(&versions[1]).unwrap();
        let third = store.get_version(&versions[2]).unwrap();
        assert_eq!(first.body.as_deref(), Some("{ wrong }"));
        assert_eq!(first.parent, None);
        assert_eq!(second.parent, Some(first.id.clone()));
        assert_eq!(third.parent, Some(second.id.clone()));
        assert!(third.origin.created_ms > second.origin.created_ms);
        assert_eq!(view.versions[&nid("a")], third.id);
        assert_eq!(
            store
                .get_version(&view.versions[&nid("a")])
                .unwrap()
                .body
                .as_deref(),
            Some("{ right }")
        );
        assert!(gen.1[1].contains("failure number 1"));
        assert!(gen.1[2].contains("failure number 1"));
        assert!(gen.1[2].contains("failure number 2"));
        let crate::version::Results { tests, .. } = store
            .results_for(&view.parent.clone().unwrap())
            .unwrap()
            .unwrap();
        assert!(tests.iter().all(|t| t.passed));
        assert_eq!(store.all_results().unwrap().len(), 3);
    }

    #[test]
    fn versioned_fill_records_build_errors_as_failures() {
        use crate::version::MemoryStore;
        let mut g = Graph::default();
        g.insert(func("a", "fn a()"));
        let mut gen = Fake(
            VecDeque::from([Ok("{ x }".to_string()), Ok("{ y }".to_string())]),
            Vec::new(),
        );
        let mut run = |g: &Graph, _: &[NodeId]| {
            if g.nodes[&nid("a")].body.as_deref() == Some("{ x }") {
                TestReport {
                    compiled: false,
                    build_errors: BTreeMap::from([(nid("a"), vec!["E0308".to_string()])]),
                    ..Default::default()
                }
            } else {
                TestReport {
                    compiled: true,
                    ..Default::default()
                }
            }
        };
        let mut store = MemoryStore::default();
        let (out, view) = fill_all_versioned(
            &mut g,
            &mut gen,
            &mut run,
            1,
            &mut store,
            &RunId("r".into()),
            "m",
            None,
        )
        .unwrap();
        assert_eq!(out.filled, vec![nid("a")]);
        assert!(gen.1[1].contains("E0308"));
        let all = store.all_results().unwrap();
        assert!(all
            .iter()
            .any(|r| r.tests.iter().any(|t| !t.passed && t.message == "E0308")));
        assert_eq!(
            store
                .get_version(&view.versions[&nid("a")])
                .unwrap()
                .body
                .as_deref(),
            Some("{ y }")
        );
    }

    #[test]
    fn versioned_fill_matches_plain_fill() {
        use crate::version::MemoryStore;
        let script = || {
            Fake(
                VecDeque::from([Ok("{ wrong }".to_string()), Ok("{ right }".to_string())]),
                Vec::new(),
            )
        };
        let mut plain = tested_graph();
        let mut g1 = script();
        let p = fill_all_tested(&mut plain, &mut g1, &mut |g, t| wrong_check(g, t, "m"), 2);
        let mut ver = tested_graph();
        let mut g2 = script();
        let mut store = MemoryStore::default();
        let (v, _) = fill_all_versioned(
            &mut ver,
            &mut g2,
            &mut |g, t| wrong_check(g, t, "m"),
            2,
            &mut store,
            &RunId("r".into()),
            "m",
            None,
        )
        .unwrap();
        assert_eq!(plain, ver);
        assert_eq!(p.filled, v.filled);
        assert_eq!(p.retries, v.retries);
    }

    #[test]
    fn prompt_includes_the_spec_when_present() {
        let mut g = Graph::default();
        g.insert(Node {
            id: NodeId("f".into()),
            kind: NodeKind::Function,
            signature: "fn f()".into(),
            body: None,
            impl_header: None,
            placement: Placement {
                file: "lib.rs".into(),
                index: 0,
            },
        });
        assert!(!build_prompt(&g, &NodeId("f".into()), &[]).contains("Specification"));
        g.spec = "multiply by 100.0 first".into();
        let p = build_prompt(&g, &NodeId("f".into()), &[]);
        assert!(p.contains("multiply by 100.0 first"));
        assert!(p.contains("fn f()"));
    }
}
