use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct NodeId(pub String);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeKind {
    Use,
    Type,
    Const,
    Function,
    Method,
    Test,
    Bench,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Placement {
    pub file: PathBuf,
    pub index: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub id: NodeId,
    pub kind: NodeKind,
    /// Item text up to the body for Function/Method/Test (`#[test] fn name()` for tests) (`pub fn f(x: u32) -> u32`);
    /// the full item text for every other kind.
    pub signature: String,
    /// Block text including braces. `None` means unfilled and renders as `{ todo!() }`.
    pub body: Option<String>,
    /// For Method nodes, the impl header they belong to (`impl Foo`, `impl Trait for Foo`).
    pub impl_header: Option<String>,
    pub placement: Placement,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Graph {
    pub nodes: BTreeMap<NodeId, Node>,
    /// `uses[a]` contains `b` when `a` depends on `b`.
    pub uses: BTreeMap<NodeId, BTreeSet<NodeId>>,
    /// The specification the graph was generated from; included in fill prompts.
    #[serde(default)]
    pub spec: String,
    /// Nodes that already exist and must never be refilled or blamed.
    #[serde(default)]
    pub frozen: BTreeSet<NodeId>,
    /// Why a node's earlier attempt was judged too large; its next prompt asks for a restructure.
    #[serde(default)]
    pub refactor: BTreeMap<NodeId, String>,
    /// How many distinct attempts any one node may use over the whole run, across
    /// every pass; 0 means unlimited.
    #[serde(default)]
    pub budget: usize,
    /// Distinct attempts used so far per node.
    #[serde(default)]
    pub spent: BTreeMap<NodeId, usize>,
    /// Repeats of an earlier answer per node; free, but capped at half the budget.
    #[serde(default)]
    pub rolled_back: BTreeMap<NodeId, usize>,
}

impl Graph {
    pub fn exhausted(&self, id: &NodeId) -> bool {
        self.budget > 0 && self.spent.get(id).copied().unwrap_or(0) >= self.budget
    }

    pub fn spend(&mut self, id: &NodeId) {
        *self.spent.entry(id.clone()).or_default() += 1;
    }
}

impl Node {
    pub fn is_filled(&self) -> bool {
        match self.kind {
            NodeKind::Function | NodeKind::Method | NodeKind::Test | NodeKind::Bench => {
                self.body.is_some()
            }
            _ => true,
        }
    }

    pub fn render(&self) -> String {
        match self.kind {
            NodeKind::Function | NodeKind::Method | NodeKind::Test | NodeKind::Bench => {
                let body = self.body.as_deref().unwrap_or("{ todo!() }");
                format!("{} {}", self.signature, body)
            }
            _ => self.signature.clone(),
        }
    }
}

impl Graph {
    pub fn insert(&mut self, node: Node) {
        self.nodes.insert(node.id.clone(), node);
    }

    pub fn add_use(&mut self, from: &NodeId, to: &NodeId) {
        self.uses
            .entry(from.clone())
            .or_default()
            .insert(to.clone());
    }

    pub fn dependencies(&self, id: &NodeId) -> impl Iterator<Item = &NodeId> {
        self.uses.get(id).into_iter().flatten()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TestOutcome {
    pub test: NodeId,
    pub passed: bool,
    pub message: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TestReport {
    pub compiled: bool,
    pub outcomes: Vec<TestOutcome>,
    /// Compile errors attributed to the node whose span contains them.
    pub build_errors: BTreeMap<NodeId, Vec<String>>,
    pub unattributed: Vec<String>,
}

impl Graph {
    /// Test nodes that exercise `id`.
    pub fn tests_for(&self, id: &NodeId) -> Vec<&NodeId> {
        self.uses
            .iter()
            .filter(|(test, deps)| {
                deps.contains(id)
                    && self
                        .nodes
                        .get(*test)
                        .is_some_and(|n| n.kind == NodeKind::Test)
            })
            .map(|(test, _)| test)
            .collect()
    }

    /// Non-test nodes that the test `id` exercises.
    pub fn subjects_of(&self, id: &NodeId) -> Vec<&NodeId> {
        self.dependencies(id)
            .filter(|dep| {
                self.nodes
                    .get(*dep)
                    .is_some_and(|n| n.kind != NodeKind::Test)
            })
            .collect()
    }
}
