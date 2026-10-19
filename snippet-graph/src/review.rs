use crate::fill::Generator;
use crate::model::{Graph, NodeId, NodeKind};
use std::collections::BTreeSet;

const SECTION_HEADINGS: [&str; 6] = [
    "# arguments",
    "# returns",
    "# errors",
    "# panics",
    "# examples",
    "# safety",
];

/// Phrases that each name one more thing a function does beyond its main job.
const DUTY_WORDS: [(&str, &[&str]); 4] = [
    (
        "ordering",
        &["ordered", "sorted", "ascending", "descending", "in order of"],
    ),
    (
        "exclusion",
        &[
            "ignored", "skips", "skipped", "excluded", "excluding", "except", "not counted",
        ],
    ),
    (
        "filtering",
        &["only if", "at least one", "that count", "which count", "whose", "filter"],
    ),
    ("empty case", &["returns zero", "zero when", "empty"]),
];

const BULLETS_AND_DUTIES_LIMIT: usize = 3;
const LONG_CONTRACT_WORDS: usize = 60;
const LONG_CONTRACT_MIN_DUTIES: usize = 2;
const MAX_PARAMETERS: usize = 5;

pub const MAX_REVIEW_ROUNDS: usize = 2;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Flag {
    pub node: NodeId,
    pub reasons: Vec<String>,
}

struct Contract {
    bullets: usize,
    duty_words: Vec<&'static str>,
    words: usize,
}

fn contract(signature: &str) -> Contract {
    let mut main: Vec<String> = Vec::new();
    for line in signature.lines().map(str::trim) {
        let Some(text) = line.strip_prefix("///") else {
            continue;
        };
        let text = text.trim().to_lowercase();
        if SECTION_HEADINGS.iter().any(|h| text.starts_with(h)) {
            break;
        }
        main.push(text);
    }
    let bullets = main
        .iter()
        .filter(|t| {
            t.starts_with("* ")
                || t.starts_with("- ")
                || t.split_once(['.', ')'])
                    .is_some_and(|(n, rest)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()) && rest.starts_with(' '))
        })
        .count();
    let joined = main.join(" ");
    let duty_words = DUTY_WORDS
        .iter()
        .filter(|(_, words)| words.iter().any(|w| joined.contains(w)))
        .map(|(name, _)| *name)
        .collect();
    Contract {
        bullets,
        duty_words,
        words: joined.split_whitespace().count(),
    }
}

fn parameter_count(signature: &str) -> usize {
    let code: String = signature
        .lines()
        .filter(|l| !l.trim_start().starts_with("///") && !l.trim_start().starts_with("#["))
        .collect::<Vec<_>>()
        .join(" ");
    let Some(open) = code.find('(') else {
        return 0;
    };
    let mut depth = 0;
    let mut commas = 0;
    let mut any = false;
    for c in code[open..].chars() {
        match c {
            '(' | '<' | '[' => depth += 1,
            ')' | '>' | ']' => {
                depth -= 1;
                if depth == 0 {
                    break;
                }
            }
            ',' if depth == 1 => commas += 1,
            c if depth >= 1 && !c.is_whitespace() => any = true,
            _ => {}
        }
    }
    if any {
        commas + 1
    } else {
        0
    }
}

/// Reasons a single node's signature and contract suggest it does too much.
/// Empty when it looks like one job.
pub fn reasons(signature: &str) -> Vec<String> {
    let c = contract(signature);
    let duties = c.bullets + c.duty_words.len();
    let mut out = Vec::new();
    if duties > BULLETS_AND_DUTIES_LIMIT
        || (c.words >= LONG_CONTRACT_WORDS && duties >= LONG_CONTRACT_MIN_DUTIES)
    {
        let mut parts = Vec::new();
        if c.bullets > 0 {
            parts.push(format!("{} listed results", c.bullets));
        }
        parts.extend(c.duty_words.iter().map(|w| w.to_string()));
        out.push(format!(
            "contract names {duties} separate duties ({}) in {} words",
            parts.join(", "),
            c.words
        ));
    }
    let params = parameter_count(signature);
    if params > MAX_PARAMETERS {
        out.push(format!("{params} parameters (limit {MAX_PARAMETERS})"));
    }
    out
}

/// Functions and methods in the skeleton that look like they do too much.
/// Frozen nodes are never flagged.
pub fn flag_graph(graph: &Graph) -> Vec<Flag> {
    graph
        .nodes
        .values()
        .filter(|n| matches!(n.kind, NodeKind::Function | NodeKind::Method))
        .filter(|n| !graph.frozen.contains(&n.id))
        .filter_map(|n| {
            let reasons = reasons(&n.signature);
            (!reasons.is_empty()).then(|| Flag {
                node: n.id.clone(),
                reasons,
            })
        })
        .collect()
}

pub fn review_prompt(spec: &str, source: &str, flags: &[Flag], problem: Option<&str>) -> String {
    let mut p = String::from(
        "Below is a Rust skeleton that compiles: every function body is `todo!()`. Some functions have contracts that bundle several separate duties, which makes them hard to implement correctly in one piece.\n\nFlagged:\n",
    );
    for f in flags {
        p.push_str(&format!("- `{}`: {}\n", f.node.0, f.reasons.join("; ")));
    }
    p.push_str(
        "\nRewrite the skeleton so each flagged function is split:\n\
- Move each distinct step (filtering, ordering, counting, summing, building one record) into its own small private free function with a `///` contract that states what it takes, what it returns and any special cases.\n\
- The flagged function keeps its exact name and signature and becomes a short sequence of calls to the helpers; give it a shorter `///` contract.\n\
- If a flagged function really is one step, leave it unchanged.\n\
- Keep every other item exactly as it is, including signatures.\n\
- Every function body, helpers included, is exactly `{ todo!() }`. No logic.\n\
- Output the complete skeleton file as Rust source only.\n",
    );
    p.push_str(&format!("\nSpec:\n{spec}\n\nSkeleton:\n```rust\n{source}\n```\n"));
    if let Some(problem) = problem {
        p.push_str(&format!("\nYour previous rewrite was rejected: {problem}\n"));
    }
    p
}

fn code_only(signature: &str) -> String {
    signature
        .lines()
        .filter(|l| !l.trim_start().starts_with("///"))
        .collect::<Vec<_>>()
        .join("\n")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Why `after` is not an acceptable rewrite of `before`, if it is not: every
/// earlier node must survive with the same signature (contracts may change).
fn rejection(before: &Graph, after: &Graph, flags: &[Flag]) -> Option<String> {
    for (id, old) in before.nodes.iter().filter(|(id, _)| !before.frozen.contains(*id)) {
        match after.nodes.get(id) {
            None => return Some(format!("`{}` is missing; keep every item", id.0)),
            Some(new) if code_only(&new.signature) != code_only(&old.signature) => {
                return Some(format!(
                    "the signature of `{}` changed; only contracts may change",
                    id.0
                ))
            }
            _ => {}
        }
    }
    let flagged: BTreeSet<&NodeId> = flags.iter().map(|f| &f.node).collect();
    let added = after.nodes.len() - before.nodes.len().min(after.nodes.len());
    if added == 0 && !flagged.is_empty() {
        return Some("nothing was split: no new helper functions were added".to_string());
    }
    None
}

pub struct Reviewed {
    pub graph: Graph,
    pub source: String,
    /// One line per round saying what was flagged and what came of it.
    pub notes: Vec<String>,
}

/// Review a compiling skeleton and split flagged functions. `build` turns
/// skeleton source into a compile-checked graph (Err carries the compiler
/// errors). A rewrite that does not compile or drops anything is retried once
/// with the problem stated, then abandoned; the earlier skeleton is kept.
pub fn review_skeleton(
    graph: Graph,
    source: String,
    spec: &str,
    generator: &mut dyn Generator,
    build: &mut dyn FnMut(&str) -> Result<Graph, Vec<String>>,
    rounds: usize,
) -> Reviewed {
    let mut current = Reviewed {
        graph,
        source,
        notes: Vec::new(),
    };
    for round in 1..=rounds {
        let flags = flag_graph(&current.graph);
        if flags.is_empty() {
            break;
        }
        let names: Vec<String> = flags.iter().map(|f| format!("`{}`", f.node.0)).collect();
        let mut problem: Option<String> = None;
        let mut accepted = false;
        for _ in 0..2 {
            let prompt = review_prompt(spec, &current.source, &flags, problem.as_deref());
            let text = match generator.generate(&prompt) {
                Ok(t) => crate::review::unfence(&t),
                Err(e) => {
                    problem = Some(e);
                    continue;
                }
            };
            match build(&text) {
                Err(errors) => problem = Some(format!("it did not compile: {}", errors.join(" "))),
                Ok(next) => match rejection(&current.graph, &next, &flags) {
                    Some(why) => problem = Some(why),
                    None => {
                        let added = next.nodes.len() - current.graph.nodes.len();
                        current.notes.push(format!(
                            "round {round}: flagged {}; split, {added} new functions",
                            names.join(", ")
                        ));
                        current.graph = next;
                        current.source = text;
                        accepted = true;
                        break;
                    }
                },
            }
        }
        if !accepted {
            current.notes.push(format!(
                "round {round}: flagged {}; no acceptable split ({}), kept the skeleton as it was",
                names.join(", "),
                problem.unwrap_or_default()
            ));
            break;
        }
    }
    current
}

/// Strip a surrounding markdown fence from model output.
pub fn unfence(text: &str) -> String {
    let t = text.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t.to_string();
    };
    let rest = rest.split_once('\n').map_or("", |(_, body)| body);
    rest.rsplit_once("```")
        .map_or(rest, |(body, _)| body)
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Node, Placement};
    use std::collections::VecDeque;

    const MONTH_SUMMARY: &str = "/// Produces a month‑wide summary across all representatives specified in `rates`.
/// For each `(rep_id, rate)` pair, a `RepStatement` is generated if that representative
/// has at least one order that counts toward the month. Statements are ordered by
/// ascending `rep_id`. The summary aggregates:
/// * `orders`: total counted orders across all included representatives,
/// * `net_sales`: sum of net sales across those statements,
/// * `commission`: sum of commissions across those statements,
/// * `unpaid_orders`: number of counted orders whose payments sum to exactly zero
///   (including orders with no payments).
///
/// Orders belonging to representatives not listed in `rates` are ignored entirely.
///
/// # Arguments
///
/// * `orders` – Slice of all orders to evaluate.
/// * `rates` – Slice of `(rep_id, commission_rate)` tuples specifying which reps to include.
///
/// # Returns
///
/// A `MonthSummary` containing the aggregated data and per‑rep statements.
pub fn month_summary(orders: &[OrderRecord], rates: &[(u32, f64)], year: i32, month: u32) -> MonthSummary";

    const NET_SALES: &str = "/// Sum of the line totals of the order after discount, before tax and shipping.
/// Returns `Decimal::ZERO` when the order has no lines.
pub fn net_sales(order: &OrderRecord) -> Decimal";

    const PROSE_ONLY: &str = "/// Produces a month-wide summary across all representatives specified in `rates`. For each pair a statement is generated if that representative has at least one order that counts toward the month, with statements ordered by ascending rep id. The summary adds the orders, net sales and commission across the statements and counts orders that are unpaid, and orders of reps missing from the list are ignored.
pub fn month_summary(orders: &[OrderRecord]) -> MonthSummary";

    #[test]
    fn a_contract_with_many_duties_is_flagged_and_a_short_one_is_not() {
        let r = reasons(MONTH_SUMMARY);
        assert_eq!(r.len(), 1);
        assert!(r[0].contains("separate duties"), "{r:?}");
        assert!(r[0].contains("4 listed results"));
        assert!(reasons(NET_SALES).is_empty());
    }

    #[test]
    fn a_long_prose_contract_with_a_few_duties_is_flagged() {
        assert!(!reasons(PROSE_ONLY).is_empty());
    }

    #[test]
    fn argument_bullets_do_not_count_as_duties() {
        let sig = "/// Adds two numbers.\n///\n/// # Arguments\n///\n/// * `a` - first\n/// * `b` - second\n/// * `c` - third\n/// * `d` - fourth\n/// * `e` - fifth\npub fn add(a: u32, b: u32) -> u32";
        assert!(reasons(sig).is_empty());
    }

    #[test]
    fn many_parameters_are_flagged() {
        let sig = "pub fn f(a: u32, b: u32, c: u32, d: u32, e: u32, g: Vec<(u32, u32)>) -> u32";
        let r = reasons(sig);
        assert_eq!(r, vec!["6 parameters (limit 5)".to_string()]);
        assert_eq!(parameter_count("pub fn f() -> u32"), 0);
        assert_eq!(parameter_count("pub fn f(a: HashMap<u32, u32>) -> u32"), 1);
    }

    fn node(id: &str, signature: &str) -> Node {
        Node {
            id: NodeId(id.into()),
            kind: NodeKind::Function,
            signature: signature.into(),
            body: Some("{ todo!() }".into()),
            impl_header: None,
            placement: Placement {
                file: "src/lib.rs".into(),
                index: 0,
            },
        }
    }

    fn graph(items: &[(&str, &str)]) -> Graph {
        let mut g = Graph::default();
        for (id, sig) in items {
            g.insert(node(id, sig));
        }
        g
    }

    struct Fake(VecDeque<String>, Vec<String>);

    impl Generator for Fake {
        fn generate(&mut self, prompt: &str) -> Result<String, String> {
            self.1.push(prompt.to_string());
            self.0.pop_front().ok_or_else(|| "exhausted".to_string())
        }
    }

    fn flagged_graph() -> Graph {
        graph(&[("month_summary", MONTH_SUMMARY), ("net_sales", NET_SALES)])
    }

    fn split_graph() -> Graph {
        graph(&[
            (
                "month_summary",
                "/// Calls the helpers.\npub fn month_summary(orders: &[OrderRecord], rates: &[(u32, f64)], year: i32, month: u32) -> MonthSummary",
            ),
            ("net_sales", NET_SALES),
            ("count_unpaid", "/// Counts unpaid orders.\nfn count_unpaid(orders: &[OrderRecord]) -> usize"),
        ])
    }

    fn month_summary_signature() -> String {
        MONTH_SUMMARY.lines().last().unwrap().to_string()
    }

    #[test]
    fn prompt_names_the_flagged_function_the_spec_and_the_skeleton() {
        let flags = flag_graph(&flagged_graph());
        assert_eq!(flags.len(), 1);
        let p = review_prompt("SPEC TEXT", "SKELETON TEXT", &flags, Some("it did not compile"));
        assert!(p.contains("`month_summary`"));
        assert!(p.contains("SPEC TEXT") && p.contains("SKELETON TEXT"));
        assert!(p.contains("rejected: it did not compile"));
        assert!(!p.contains("`net_sales`:"));
    }

    #[test]
    fn a_good_rewrite_is_accepted_and_noted() {
        let mut gen = Fake(VecDeque::from(["```rust\nNEW SKELETON\n```".to_string()]), Vec::new());
        let mut seen = Vec::new();
        let mut build = |text: &str| {
            seen.push(text.to_string());
            Ok(split_graph())
        };
        let out = review_skeleton(flagged_graph(), "OLD".into(), "spec", &mut gen, &mut build, 2);
        assert_eq!(out.graph.nodes.len(), 3);
        assert_eq!(out.source, "NEW SKELETON");
        assert_eq!(seen, vec!["NEW SKELETON".to_string()]);
        assert_eq!(out.notes.len(), 1);
        assert!(out.notes[0].contains("`month_summary`") && out.notes[0].contains("1 new functions"));
        assert_eq!(gen.1.len(), 1, "round two finds nothing flagged and asks nothing");
    }

    #[test]
    fn a_rewrite_that_does_not_compile_is_retried_with_the_errors_then_abandoned() {
        let mut gen = Fake(VecDeque::from(["A".to_string(), "B".to_string()]), Vec::new());
        let mut build = |_: &str| Err(vec!["error[E0425]: cannot find value".to_string()]);
        let out = review_skeleton(flagged_graph(), "OLD".into(), "spec", &mut gen, &mut build, 2);
        assert_eq!(out.source, "OLD");
        assert_eq!(out.graph.nodes.len(), 2);
        assert!(gen.1[1].contains("did not compile: error[E0425]"));
        assert!(out.notes[0].contains("kept the skeleton as it was"));
    }

    #[test]
    fn a_rewrite_that_drops_or_changes_an_item_is_rejected() {
        let missing = graph(&[("month_summary", &month_summary_signature())]);
        let mut gen = Fake(VecDeque::from(["A".to_string(), "B".to_string()]), Vec::new());
        let mut build = |_: &str| Ok(missing.clone());
        let out = review_skeleton(flagged_graph(), "OLD".into(), "spec", &mut gen, &mut build, 1);
        assert_eq!(out.source, "OLD");
        assert!(gen.1[1].contains("`net_sales` is missing"));

        let mut changed = split_graph();
        changed.nodes.get_mut(&NodeId("net_sales".into())).unwrap().signature =
            "pub fn net_sales(order: &OrderRecord, extra: u32) -> Decimal".into();
        let mut gen = Fake(VecDeque::from(["A".to_string(), "B".to_string()]), Vec::new());
        let mut build = |_: &str| Ok(changed.clone());
        let out = review_skeleton(flagged_graph(), "OLD".into(), "spec", &mut gen, &mut build, 1);
        assert_eq!(out.source, "OLD");
        assert!(gen.1[1].contains("signature of `net_sales` changed"));
    }

    #[test]
    fn a_rewrite_that_splits_nothing_is_rejected() {
        let same = flagged_graph();
        let mut gen = Fake(VecDeque::from(["A".to_string(), "B".to_string()]), Vec::new());
        let mut build = |_: &str| Ok(same.clone());
        let out = review_skeleton(flagged_graph(), "OLD".into(), "spec", &mut gen, &mut build, 1);
        assert!(gen.1[1].contains("nothing was split"));
        assert_eq!(out.notes.len(), 1);
    }

    #[test]
    fn a_clean_skeleton_costs_no_model_call() {
        let mut gen = Fake(VecDeque::new(), Vec::new());
        let mut build = |_: &str| Ok(Graph::default());
        let clean = graph(&[("net_sales", NET_SALES)]);
        let out = review_skeleton(clean, "OLD".into(), "spec", &mut gen, &mut build, 2);
        assert!(gen.1.is_empty() && out.notes.is_empty());
    }

    #[test]
    fn frozen_nodes_are_never_flagged() {
        let mut g = flagged_graph();
        g.frozen.insert(NodeId("month_summary".into()));
        assert!(flag_graph(&g).is_empty());
    }
}
