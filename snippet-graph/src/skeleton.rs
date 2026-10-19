use crate::model::{Graph, Node, NodeId, NodeKind, Placement};
use proc_macro2::{LineColumn, TokenStream, TokenTree};
use quote::ToTokens;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::str::FromStr;
use syn::{ImplItem, Item, Stmt};

#[derive(Debug, thiserror::Error)]
pub enum SkeletonError {
    #[error("parse error in {file}: {message}")]
    Parse { file: String, message: String },
}

/// Parse a skeleton (`todo!()` bodies) into a graph. `file` becomes each node's
/// placement; item order in the source becomes `Placement::index`. Edges are
/// derived from identifiers each node mentions that match another node's name.
pub fn parse(file: &Path, source: &str) -> Result<Graph, SkeletonError> {
    let ast = syn::parse_file(source).map_err(|e| SkeletonError::Parse {
        file: file.display().to_string(),
        message: e.to_string(),
    })?;
    let text = Source::new(source);
    let mut graph = Graph::default();
    let mut index = 0usize;
    let mut push = |graph: &mut Graph, id: String, kind, signature, body, impl_header| {
        let id = NodeId(id);
        graph.insert(Node {
            id,
            kind,
            signature,
            body,
            impl_header,
            placement: Placement {
                file: file.to_path_buf(),
                index,
            },
        });
        index += 1;
    };

    for item in &ast.items {
        match item {
            Item::Fn(f) => {
                let (signature, body) = split_fn(&text, f, &f.block);
                push(
                    &mut graph,
                    f.sig.ident.to_string(),
                    NodeKind::Function,
                    signature,
                    body,
                    None,
                );
            }
            Item::Struct(i) => push_whole(
                &mut graph,
                &mut push,
                &text,
                i.ident.to_string(),
                NodeKind::Type,
                item,
            ),
            Item::Enum(i) => push_whole(
                &mut graph,
                &mut push,
                &text,
                i.ident.to_string(),
                NodeKind::Type,
                item,
            ),
            Item::Union(i) => push_whole(
                &mut graph,
                &mut push,
                &text,
                i.ident.to_string(),
                NodeKind::Type,
                item,
            ),
            Item::Trait(i) => push_whole(
                &mut graph,
                &mut push,
                &text,
                i.ident.to_string(),
                NodeKind::Type,
                item,
            ),
            Item::Type(i) => push_whole(
                &mut graph,
                &mut push,
                &text,
                i.ident.to_string(),
                NodeKind::Type,
                item,
            ),
            Item::Const(i) => push_whole(
                &mut graph,
                &mut push,
                &text,
                i.ident.to_string(),
                NodeKind::Const,
                item,
            ),
            Item::Static(i) => push_whole(
                &mut graph,
                &mut push,
                &text,
                i.ident.to_string(),
                NodeKind::Const,
                item,
            ),
            Item::Use(_) => {
                let t = text.item(item);
                let id = t.split_whitespace().collect::<Vec<_>>().join(" ");
                push(&mut graph, id, NodeKind::Use, t, None, None);
            }
            Item::Impl(imp) => {
                let self_name = type_name(&imp.self_ty);
                let trait_name = imp
                    .trait_
                    .as_ref()
                    .and_then(|(_, p, _)| p.segments.last())
                    .map(|s| s.ident.to_string());
                let header_end = imp.brace_token.span.open().start();
                let header = text
                    .slice(first_start(item), header_end)
                    .trim_end()
                    .to_string();
                for it in &imp.items {
                    if let ImplItem::Fn(m) = it {
                        let (signature, body) = split_fn(&text, m, &m.block);
                        let (signature, body) = (dedent(&signature), body.map(|b| dedent(&b)));
                        let name = m.sig.ident.to_string();
                        let id = match &trait_name {
                            Some(t) => format!("{self_name}::{t}::{name}"),
                            None => format!("{self_name}::{name}"),
                        };
                        push(
                            &mut graph,
                            id,
                            NodeKind::Method,
                            signature,
                            body,
                            Some(header.clone()),
                        );
                    }
                }
            }
            Item::Mod(m) if has_attr(&m.attrs, "cfg", Some("test")) => {
                let bench = m.ident == "benches";
                for it in m.content.iter().flat_map(|(_, items)| items) {
                    if let Item::Fn(f) = it {
                        if !has_attr(&f.attrs, "test", None) {
                            continue;
                        }
                        let (signature, body) = split_fn(&text, &f.sig, &f.block);
                        let (signature, body) = (dedent(&signature), body.map(|b| dedent(&b)));
                        let name = f.sig.ident.to_string();
                        let prefix = if bench { "bench::" } else { "test::" };
                        let id = if name.starts_with(prefix) {
                            name
                        } else {
                            format!("{prefix}{name}")
                        };
                        push(
                            &mut graph,
                            id,
                            if bench {
                                NodeKind::Bench
                            } else {
                                NodeKind::Test
                            },
                            format!("#[test] {signature}"),
                            Some(body.unwrap_or_else(|| dedent(&text.block(&f.block)))),
                            None,
                        );
                    }
                }
            }
            _ => {}
        }
    }

    link(&mut graph);
    Ok(graph)
}

/// Merge another file's graph into `into`, recomputing cross-file edges.
pub fn merge(into: &mut Graph, other: Graph) {
    into.nodes.extend(other.nodes);
    link(into);
}

fn push_whole<F>(
    graph: &mut Graph,
    push: &mut F,
    text: &Source,
    id: String,
    kind: NodeKind,
    item: &Item,
) where
    F: FnMut(&mut Graph, String, NodeKind, String, Option<String>, Option<String>),
{
    push(graph, id, kind, text.item(item), None, None);
}

fn link(graph: &mut Graph) {
    let names: BTreeMap<&str, &NodeId> = graph
        .nodes
        .iter()
        .filter(|(_, n)| {
            matches!(
                n.kind,
                NodeKind::Function | NodeKind::Type | NodeKind::Const
            )
        })
        .map(|(id, _)| (id.0.as_str(), id))
        .collect();

    let mut uses: BTreeMap<NodeId, BTreeSet<NodeId>> = BTreeMap::new();
    for (id, node) in &graph.nodes {
        let mut text = node.impl_header.clone().unwrap_or_default();
        text.push(' ');
        text.push_str(&node.render());
        let deps: BTreeSet<NodeId> = identifiers(&text)
            .into_iter()
            .filter_map(|ident| names.get(ident.as_str()).copied())
            .filter(|dep| *dep != id)
            .cloned()
            .collect();
        if !deps.is_empty() {
            uses.insert(id.clone(), deps);
        }
    }
    graph.uses = uses;
    crate::edges::resolve(graph);
}

fn has_attr(attrs: &[syn::Attribute], name: &str, arg: Option<&str>) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident(name)
            && match arg {
                None => true,
                Some(arg) => a.parse_args::<syn::Ident>().is_ok_and(|i| i == arg),
            }
    })
}

fn identifiers(text: &str) -> BTreeSet<String> {
    fn walk(ts: TokenStream, out: &mut BTreeSet<String>) {
        for tt in ts {
            match tt {
                TokenTree::Ident(i) => {
                    out.insert(i.to_string());
                }
                TokenTree::Group(g) => walk(g.stream(), out),
                _ => {}
            }
        }
    }
    let mut out = BTreeSet::new();
    if let Ok(ts) = TokenStream::from_str(text) {
        walk(ts, &mut out);
    }
    out
}

fn type_name(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(p) => p
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_default(),
        syn::Type::Reference(r) => type_name(&r.elem),
        other => other.to_token_stream().to_string(),
    }
}

fn first_start(item: &impl ToTokens) -> LineColumn {
    item.to_token_stream()
        .into_iter()
        .next()
        .map(|t| t.span().start())
        .unwrap_or(LineColumn { line: 1, column: 0 })
}

fn last_end(item: &impl ToTokens) -> LineColumn {
    item.to_token_stream()
        .into_iter()
        .last()
        .map(|t| t.span().end())
        .unwrap_or(LineColumn { line: 1, column: 0 })
}

/// assemble indents nested items by four spaces, so undo that here for round-trip stability.
fn dedent(text: &str) -> String {
    let inside_literal = crate::assemble::literal_continuation_lines(text);
    let mut lines = text.lines().enumerate();
    let mut out = lines.next().map(|(_, l)| l).unwrap_or_default().to_string();
    for (i, line) in lines {
        out.push('\n');
        if inside_literal.contains(&i) {
            out.push_str(line);
        } else {
            out.push_str(line.strip_prefix("    ").unwrap_or(line));
        }
    }
    out
}

fn split_fn(text: &Source, item: &impl ToTokens, block: &syn::Block) -> (String, Option<String>) {
    let open = block.brace_token.span.open().start();
    let close = block.brace_token.span.close().end();
    let signature = text.slice(first_start(item), open).trim_end().to_string();
    let body = if is_todo(block) {
        None
    } else {
        Some(text.slice(open, close).to_string())
    };
    (signature, body)
}

fn is_todo(block: &syn::Block) -> bool {
    match block.stmts.as_slice() {
        [Stmt::Macro(m)] => is_empty_todo(&m.mac),
        [Stmt::Expr(syn::Expr::Macro(m), _)] => is_empty_todo(&m.mac),
        _ => false,
    }
}

fn is_empty_todo(mac: &syn::Macro) -> bool {
    mac.path.is_ident("todo") && mac.tokens.is_empty()
}

struct Source<'a> {
    src: &'a str,
    line_starts: Vec<usize>,
}

impl<'a> Source<'a> {
    fn new(src: &'a str) -> Self {
        let mut line_starts = vec![0];
        line_starts.extend(src.match_indices('\n').map(|(i, _)| i + 1));
        Source { src, line_starts }
    }

    fn offset(&self, lc: LineColumn) -> usize {
        let start = self.line_starts[(lc.line - 1).min(self.line_starts.len() - 1)];
        let rest = &self.src[start..];
        let line_len = rest.find('\n').unwrap_or(rest.len());
        rest[..line_len]
            .char_indices()
            .nth(lc.column)
            .map(|(i, _)| start + i)
            .unwrap_or(start + line_len)
    }

    fn slice(&self, from: LineColumn, to: LineColumn) -> &'a str {
        &self.src[self.offset(from)..self.offset(to)]
    }

    fn block(&self, block: &syn::Block) -> String {
        self.slice(
            block.brace_token.span.open().start(),
            block.brace_token.span.close().end(),
        )
        .to_string()
    }

    fn item(&self, item: &impl ToTokens) -> String {
        self.slice(first_start(item), last_end(item)).to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"
use std::path::Path;

/// A thing.
pub struct Foo {
    pub x: u32,
}

pub const LIMIT: usize = 3;

pub fn make(x: u32) -> Foo {
    todo!()
}

fn filled() -> usize {
    LIMIT + 1
}

impl Foo {
    pub fn new() -> Self {
        todo!()
    }
    pub fn go(&self) -> usize {
        filled()
    }
}

impl Default for Foo {
    fn default() -> Self {
        Foo::new()
    }
}
"#;

    fn id(s: &str) -> NodeId {
        NodeId(s.to_string())
    }

    #[test]
    fn nodes_and_kinds() {
        let g = parse(Path::new("a.rs"), SRC).unwrap();
        assert_eq!(g.nodes[&id("Foo")].kind, NodeKind::Type);
        assert!(g.nodes[&id("Foo")].signature.starts_with("/// A thing."));
        assert_eq!(g.nodes[&id("LIMIT")].kind, NodeKind::Const);
        assert_eq!(g.nodes[&id("use std::path::Path;")].kind, NodeKind::Use);
        assert_eq!(g.nodes[&id("Foo::new")].kind, NodeKind::Method);
        assert!(g.nodes.contains_key(&id("Foo::Default::default")));
    }

    #[test]
    fn signatures_and_bodies() {
        let g = parse(Path::new("a.rs"), SRC).unwrap();
        let make = &g.nodes[&id("make")];
        assert_eq!(make.signature, "pub fn make(x: u32) -> Foo");
        assert_eq!(make.body, None);
        let filled = &g.nodes[&id("filled")];
        assert_eq!(filled.signature, "fn filled() -> usize");
        assert!(filled.body.as_ref().unwrap().contains("LIMIT + 1"));
        let go = &g.nodes[&id("Foo::go")];
        assert_eq!(go.impl_header.as_deref(), Some("impl Foo"));
        assert_eq!(
            g.nodes[&id("Foo::Default::default")].impl_header.as_deref(),
            Some("impl Default for Foo")
        );
        assert_eq!(g.nodes[&id("Foo::new")].body, None);
    }

    #[test]
    fn edges_and_placement() {
        let g = parse(Path::new("a.rs"), SRC).unwrap();
        let deps =
            |n: &str| -> Vec<String> { g.dependencies(&id(n)).map(|d| d.0.clone()).collect() };
        assert_eq!(deps("make"), vec!["Foo"]);
        assert_eq!(deps("filled"), vec!["LIMIT"]);
        assert_eq!(deps("Foo::go"), vec!["Foo", "filled"]);
        assert_eq!(g.nodes[&id("Foo")].placement.index, 1);
        assert_eq!(g.nodes[&id("Foo")].placement.file, Path::new("a.rs"));
    }

    #[test]
    fn merge_links_across_files() {
        let mut a = parse(Path::new("a.rs"), "pub fn f() -> B { todo!() }").unwrap();
        let b = parse(Path::new("b.rs"), "pub struct B;").unwrap();
        assert_eq!(a.dependencies(&id("f")).count(), 0);
        merge(&mut a, b);
        assert_eq!(a.dependencies(&id("f")).next(), Some(&id("B")));
    }

    #[test]
    fn test_module_becomes_test_nodes() {
        let src = format!(
            "{SRC}\n#[cfg(test)]\nmod tests {{\n    use super::*;\n\n    fn helper() {{}}\n\n    #[test]\n    fn goes() {{\n        assert_eq!(filled(), 4);\n        make(1);\n    }}\n\n    #[test]\n    fn todo_body() {{\n        todo!()\n    }}\n}}\n"
        );
        let g = parse(Path::new("a.rs"), &src).unwrap();
        let t = &g.nodes[&id("test::goes")];
        assert_eq!(t.kind, NodeKind::Test);
        assert_eq!(t.signature, "#[test] fn goes()");
        assert!(t.body.as_ref().unwrap().contains("filled()"));
        assert!(!g.nodes.contains_key(&id("test::helper")));
        assert_eq!(
            g.nodes[&id("test::todo_body")].body.as_deref(),
            Some("{\n    todo!()\n}")
        );
        assert_eq!(
            g.subjects_of(&id("test::goes")),
            vec![&id("filled"), &id("make")]
        );
        assert_eq!(g.tests_for(&id("filled")), vec![&id("test::goes")]);
        assert!(g.tests_for(&id("Foo::go")).is_empty());
    }

    #[test]
    fn bench_module_round_trips() {
        let src = format!(
            "{SRC}\n#[cfg(test)]\nmod tests {{\n    use super::*;\n\n    #[test]\n    fn goes() {{\n        make(1);\n    }}\n}}\n\n#[cfg(test)]\nmod benches {{\n    use super::*;\n\n    #[test]\n    fn speed() {{\n        filled();\n    }}\n}}\n"
        );
        let g = parse(Path::new("a.rs"), &src).unwrap();
        let b = &g.nodes[&id("bench::speed")];
        assert_eq!(b.kind, NodeKind::Bench);
        assert_eq!(b.signature, "#[test] fn speed()");
        assert!(b.body.is_some());
        assert_eq!(g.nodes[&id("test::goes")].kind, NodeKind::Test);
        assert_eq!(
            g.dependencies(&id("bench::speed")).collect::<Vec<_>>(),
            vec![&id("filled")]
        );
        let text = crate::assemble::assemble(&g)
            .files
            .values()
            .next()
            .unwrap()
            .clone();
        assert!(text.find("mod tests").unwrap() < text.find("mod benches").unwrap());
        let g2 = parse(Path::new("a.rs"), &text).unwrap();
        assert_eq!(g2.nodes, g.nodes);
        assert_eq!(g2.uses, g.uses);
    }

    #[test]
    fn parse_error() {
        assert!(parse(Path::new("a.rs"), "fn (").is_err());
    }

    #[test]
    fn method_bodies_round_trip_stably() {
        let src = "impl Foo {\n    pub fn go(&self) -> u32 {\n        let x = 1;\n        x + 1\n    }\n}\n";
        let once = crate::assemble::assemble(&parse(Path::new("a.rs"), src).unwrap());
        let text = once.files.values().next().unwrap().clone();
        let twice = crate::assemble::assemble(&parse(Path::new("a.rs"), &text).unwrap());
        assert_eq!(twice.files.values().next().unwrap(), &text);
    }

    #[test]
    fn string_literal_contents_survive_round_trip() {
        let src = "impl Foo {\n    pub fn go(&self) -> String {\n        \"one\ntwo\".to_string()\n    }\n}\n";
        let once = crate::assemble::assemble(&parse(Path::new("a.rs"), src).unwrap());
        let text = once.files.values().next().unwrap().clone();
        assert!(text.contains("\"one\ntwo\""));
        let twice = crate::assemble::assemble(&parse(Path::new("a.rs"), &text).unwrap());
        assert_eq!(twice.files.values().next().unwrap(), &text);
    }
}
