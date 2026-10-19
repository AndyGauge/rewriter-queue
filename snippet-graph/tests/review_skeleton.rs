use snippet_graph::assemble::assemble;
use snippet_graph::fill::Generator;
use snippet_graph::review::review_skeleton;
use snippet_graph::skeleton;
use snippet_graph::verify;
use snippet_graph::{Graph, NodeId};
use std::path::Path;

const BUNDLED: &str = "pub struct Summary { pub orders: usize, pub unpaid: usize }

/// Summarises a month across the listed reps. A statement is produced for a rep only if
/// that rep has at least one order that counts toward the month; statements are ordered by
/// ascending rep id. The summary adds up:
/// * `orders`: counted orders,
/// * `unpaid`: counted orders with no payments,
/// * `net`: sum of net sales,
/// * `commission`: sum of commissions.
/// Orders of reps missing from the list are ignored entirely.
pub fn summarise(orders: &[u32], reps: &[u32]) -> Summary { todo!() }
";

const SPLIT: &str = "pub struct Summary { pub orders: usize, pub unpaid: usize }

/// Counted orders belonging to listed reps: the order value is the rep id here.
fn counted(orders: &[u32], reps: &[u32]) -> Vec<u32> { todo!() }

/// Number of counted orders with no payments (an order with no payments counts).
fn count_unpaid(counted: &[u32]) -> usize { todo!() }

/// Calls the helpers and builds the summary.
pub fn summarise(orders: &[u32], reps: &[u32]) -> Summary { todo!() }
";

struct Fake(Vec<String>, usize);

impl Generator for Fake {
    fn generate(&mut self, prompt: &str) -> Result<String, String> {
        self.1 += 1;
        assert!(prompt.contains("`summarise`"), "prompt must name the flagged function");
        Ok(self.0.remove(0))
    }
}

fn build(dir: &Path, text: &str) -> Result<Graph, Vec<String>> {
    let graph = skeleton::parse(Path::new("src/lib.rs"), text).map_err(|e| vec![e.to_string()])?;
    let report = verify::check(dir, &assemble(&graph)).map_err(|e| vec![e.to_string()])?;
    if report.ok {
        Ok(graph)
    } else {
        Err(report.by_node.into_values().flatten().chain(report.unattributed).collect())
    }
}

#[test]
fn review_splits_a_bundled_function_with_the_real_parser_and_compiler() {
    let dir = std::env::temp_dir().join(format!("sg_review_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"review_check\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n",
    )
    .unwrap();

    let first = build(&dir, BUNDLED).expect("the bundled skeleton compiles");
    let mut gen = Fake(vec![format!("```rust\n{SPLIT}\n```")], 0);
    let reviewed = review_skeleton(
        first,
        BUNDLED.to_string(),
        "spec",
        &mut gen,
        &mut |text| build(&dir, text),
        2,
    );

    assert_eq!(gen.1, 1, "one rewrite, and the split skeleton is no longer flagged");
    assert_eq!(reviewed.graph.nodes.len(), 4);
    assert!(reviewed.graph.nodes.contains_key(&NodeId("counted".into())));
    assert!(reviewed.graph.nodes.contains_key(&NodeId("count_unpaid".into())));
    assert!(reviewed.graph.nodes[&NodeId("summarise".into())]
        .signature
        .contains("Calls the helpers"));
    assert_eq!(reviewed.notes.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}
