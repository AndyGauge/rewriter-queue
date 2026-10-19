use crate::fill::Generator;
use crate::model::{Graph, Node, NodeId, NodeKind, Placement};
use proc_macro2::{TokenStream, TokenTree};
use std::collections::BTreeSet;

const BENCH_INDEX_BASE: usize = 2_000_000;

/// Prompt asking for `#[test] fn bench_<name>()` timing functions for one
/// subject, from the spec and the signatures (never bodies) of the subject and
/// its dependencies.
pub fn build_prompt(spec: &str, graph: &Graph, subject: &NodeId, errors: &[String]) -> String {
    let mut out = String::new();
    out.push_str(
        "Write Rust benchmarks for one function, derived only from the specification below.\n",
    );
    out.push_str("Rules:\n");
    out.push_str("- Respond with only `#[test] fn bench_<name>() { ... }` functions; each must be self-contained (no helper items).\n");
    out.push_str(
        "- Each bench times a realistic workload for the function with `std::time::Instant`.\n",
    );
    out.push_str("- Build the input outside the timed region, run the function under test repeatedly inside it, and pass inputs and results through `std::hint::black_box`.\n");
    out.push_str("- Finish each bench with `println!(\"BENCH {}\", elapsed_ns);` where `elapsed_ns` is the elapsed nanoseconds of the timed region.\n");
    out.push_str("- Do not assert on performance; the bench only reports a time.\n");
    out.push_str("- Do not rely on any other function being correct beyond its signature.\n");
    out.push_str("- Anything touching the filesystem must use a unique directory under `std::env::temp_dir()` (include the bench name and process id), never a shared path.\n");
    out.push_str("- The benches run inside a module with `use super::*;`.\n");
    out.push_str("\nSpecification:\n");
    out.push_str(spec);
    out.push('\n');

    if let Some(node) = graph.nodes.get(subject) {
        out.push_str("\nFunction to benchmark (signature only):\n```rust\n");
        if let Some(header) = &node.impl_header {
            out.push_str(&format!("// in {header}\n"));
        }
        out.push_str(&node.signature);
        out.push_str("\n```\n");

        let deps: Vec<&Node> = graph
            .dependencies(subject)
            .filter_map(|d| graph.nodes.get(d))
            .filter(|n| !matches!(n.kind, NodeKind::Test | NodeKind::Bench))
            .collect();
        if !deps.is_empty() {
            out.push_str("\nRelated items (signatures only):\n```rust\n");
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
    if !errors.is_empty() {
        out.push_str("\nYour previous attempt was rejected; fix these problems:\n");
        for e in errors {
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

fn collect_tests<'a>(items: &'a [syn::Item], out: &mut Vec<&'a syn::ItemFn>) {
    for item in items {
        match item {
            syn::Item::Fn(f) if f.attrs.iter().any(|a| a.path().is_ident("test")) => out.push(f),
            syn::Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    collect_tests(inner, out);
                }
            }
            _ => {}
        }
    }
}

fn byte_offset(src: &str, line: usize, column: usize) -> Option<usize> {
    let mut start = 0;
    for (i, l) in src.split_inclusive('\n').enumerate() {
        if i + 1 == line {
            let off = l.char_indices().nth(column).map_or(l.len(), |(b, _)| b);
            return Some(start + off);
        }
        start += l.len();
    }
    None
}

fn block_text(src: &str, block: &syn::Block) -> String {
    let open = block.brace_token.span.open().start();
    let close = block.brace_token.span.close().end();
    byte_offset(src, open.line, open.column)
        .zip(byte_offset(src, close.line, close.column))
        .filter(|(a, b)| a < b)
        .map(|(a, b)| src[a..b].to_string())
        .unwrap_or_else(|| quote::quote!(#block).to_string())
}

/// Parse a response into Bench nodes with ids `bench::<subject>::<name>` and
/// edges to the subject, each timing a realistic workload and printing
/// `BENCH <ns>`.
pub fn parse_benches(graph: &Graph, subject: &NodeId, response: &str) -> Result<Vec<Node>, String> {
    let subject_node = graph
        .nodes
        .get(subject)
        .ok_or_else(|| format!("unknown subject {}", subject.0))?;
    let src = unfenced(response);
    let file = syn::parse_file(src).map_err(|e| format!("benches do not parse: {e}"))?;
    let mut fns = Vec::new();
    collect_tests(&file.items, &mut fns);
    if fns.is_empty() {
        return Err("no `#[test]` function found in response".into());
    }
    let existing = graph
        .nodes
        .values()
        .filter(|n| n.placement.file == subject_node.placement.file)
        .count();
    let mut seen = BTreeSet::new();
    let mut nodes = Vec::new();
    for f in fns {
        let name = f.sig.ident.to_string();
        if !seen.insert(name.clone()) {
            return Err(format!("duplicate bench name `{name}`"));
        }
        nodes.push(Node {
            id: NodeId(format!("bench::{}::{}", subject.0, name)),
            kind: NodeKind::Bench,
            signature: format!("#[test] fn {name}()"),
            body: Some(block_text(src, &f.block)),
            impl_header: None,
            placement: Placement {
                file: subject_node.placement.file.clone(),
                index: BENCH_INDEX_BASE + existing + nodes.len(),
            },
        });
    }
    Ok(nodes)
}

fn idents(ts: TokenStream, out: &mut BTreeSet<String>) {
    for tt in ts {
        match tt {
            TokenTree::Ident(i) => {
                out.insert(i.to_string());
            }
            TokenTree::Group(g) => idents(g.stream(), out),
            _ => {}
        }
    }
}

fn link_bench(graph: &mut Graph, bench: &Node, subject: &NodeId) {
    let mut used = BTreeSet::new();
    if let Some(ts) = bench
        .body
        .as_ref()
        .and_then(|b| b.parse::<TokenStream>().ok())
    {
        idents(ts, &mut used);
    }
    let called: Vec<NodeId> = graph
        .nodes
        .values()
        .filter(|n| !matches!(n.kind, NodeKind::Test | NodeKind::Bench | NodeKind::Use))
        .filter(|n| {
            n.id.0
                .rsplit("::")
                .next()
                .is_some_and(|last| used.contains(last))
        })
        .map(|n| n.id.clone())
        .collect();
    graph.add_use(&bench.id, subject);
    for id in called {
        graph.add_use(&bench.id, &id);
    }
}

fn remove_benches(graph: &mut Graph, ids: &[NodeId]) {
    for id in ids {
        graph.nodes.remove(id);
        graph.uses.remove(id);
    }
}

/// Generate benches for `subject`; `check` must confirm they compile and run
/// against the current implementation. Retries with the reason up to `max_retries`.
pub fn generate(
    graph: &mut Graph,
    spec: &str,
    subject: &NodeId,
    generator: &mut dyn Generator,
    check: &mut dyn FnMut(&Graph) -> Result<(), String>,
    max_retries: usize,
) -> Result<(), String> {
    let mut errors: Vec<String> = Vec::new();
    for _ in 0..=max_retries {
        let prompt = build_prompt(spec, graph, subject, &errors);
        let nodes = match generator
            .generate(&prompt)
            .and_then(|r| parse_benches(graph, subject, &r))
        {
            Ok(nodes) => nodes,
            Err(e) => {
                errors = vec![e];
                continue;
            }
        };
        let ids: Vec<NodeId> = nodes.iter().map(|n| n.id.clone()).collect();
        for node in &nodes {
            graph.insert(node.clone());
            link_bench(graph, node, subject);
        }
        match check(graph) {
            Ok(()) => return Ok(()),
            Err(reason) => {
                remove_benches(graph, &ids);
                errors = vec![reason];
            }
        }
    }
    Err(errors.join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn func(name: &str, sig: &str, body: Option<&str>) -> Node {
        Node {
            id: NodeId(name.into()),
            kind: NodeKind::Function,
            signature: sig.into(),
            body: body.map(Into::into),
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
        g.insert(func("add", "pub fn add(a: u32, b: u32) -> u32", None));
        g.insert(func("helper", "fn helper() -> u32", Some("{ SECRET }")));
        g.add_use(&nid("add"), &nid("helper"));
        g
    }

    const GOOD: &str = "```rust\n#[test]\nfn bench_add() {\n    let t = std::time::Instant::now();\n    for _ in 0..1000 {\n        std::hint::black_box(add(1, 2));\n    }\n    println!(\"BENCH {}\", t.elapsed().as_nanos());\n}\n```";

    #[test]
    fn prompt_has_signatures_not_bodies() {
        let p = build_prompt("adds numbers", &graph(), &nid("add"), &["boom".into()]);
        assert!(p.contains("adds numbers"));
        assert!(p.contains("pub fn add(a: u32, b: u32) -> u32"));
        assert!(p.contains("fn helper() -> u32"));
        assert!(!p.contains("SECRET"));
        assert!(p.contains("boom"));
        assert!(p.contains("black_box"));
        assert!(p.contains("BENCH {}"));
        assert!(p.contains("temp_dir"));
    }

    #[test]
    fn parse_extracts_bench_nodes() {
        let nodes = parse_benches(&graph(), &nid("add"), GOOD).unwrap();
        assert_eq!(nodes.len(), 1);
        let n = &nodes[0];
        assert_eq!(n.id, nid("bench::add::bench_add"));
        assert_eq!(n.kind, NodeKind::Bench);
        assert_eq!(n.signature, "#[test] fn bench_add()");
        assert!(n.body.as_deref().unwrap().starts_with('{'));
        assert!(n.body.as_deref().unwrap().contains("black_box(add(1, 2))"));
        assert!(n.placement.index >= BENCH_INDEX_BASE);
    }

    #[test]
    fn parse_rejects_missing_benches_and_bad_syntax() {
        assert!(parse_benches(&graph(), &nid("add"), "fn x() {}").is_err());
        assert!(parse_benches(&graph(), &nid("add"), "#[test] fn x( {").is_err());
    }

    #[test]
    fn generate_retries_on_failed_check_then_accepts() {
        let mut g = graph();
        let mut gen = Fake(
            VecDeque::from([Ok(GOOD.to_string()), Ok(GOOD.to_string())]),
            Vec::new(),
        );
        let mut calls = 0;
        let mut check = |_: &Graph| {
            calls += 1;
            if calls == 1 {
                Err("E0425 nope".to_string())
            } else {
                Ok(())
            }
        };
        generate(&mut g, "spec", &nid("add"), &mut gen, &mut check, 2).unwrap();
        assert!(gen.1[1].contains("E0425 nope"));
        assert!(g.nodes.contains_key(&nid("bench::add::bench_add")));
        assert!(g
            .dependencies(&nid("bench::add::bench_add"))
            .any(|d| d == &nid("add")));
    }

    #[test]
    fn generate_failure_leaves_graph_clean() {
        let mut g = graph();
        let before = g.clone();
        let mut gen = Fake(
            VecDeque::from([Ok(GOOD.to_string()), Ok(GOOD.to_string())]),
            Vec::new(),
        );
        let mut check = |_: &Graph| Err("no BENCH line".to_string());
        let err = generate(&mut g, "spec", &nid("add"), &mut gen, &mut check, 1).unwrap_err();
        assert!(err.contains("no BENCH line"));
        assert_eq!(g, before);
    }

    #[test]
    fn generate_retries_after_parse_failure() {
        let mut g = graph();
        let mut gen = Fake(
            VecDeque::from([Ok("not rust (".to_string()), Ok(GOOD.to_string())]),
            Vec::new(),
        );
        let mut check = |_: &Graph| Ok(());
        generate(&mut g, "spec", &nid("add"), &mut gen, &mut check, 1).unwrap();
        assert!(gen.1[1].contains("do not parse"));
        assert!(g.nodes.contains_key(&nid("bench::add::bench_add")));
    }
}
