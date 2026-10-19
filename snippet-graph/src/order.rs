use crate::model::{Graph, NodeId, NodeKind};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, thiserror::Error)]
#[error("dependency cycle among: {0:?}")]
pub struct Cycle(pub Vec<NodeId>);

struct Condensed {
    members: Vec<Vec<NodeId>>,
    deps: Vec<BTreeSet<usize>>,
}

#[derive(Default)]
struct TarjanState {
    counter: usize,
    index: BTreeMap<NodeId, usize>,
    low: BTreeMap<NodeId, usize>,
    stack: Vec<NodeId>,
    on_stack: BTreeSet<NodeId>,
}

fn live_deps<'a>(graph: &'a Graph, id: &NodeId) -> impl Iterator<Item = &'a NodeId> {
    graph
        .dependencies(id)
        .filter(|d| graph.nodes.contains_key(*d))
}

fn strongconnect(graph: &Graph, v: &NodeId, st: &mut TarjanState, out: &mut Vec<Vec<NodeId>>) {
    st.index.insert(v.clone(), st.counter);
    st.low.insert(v.clone(), st.counter);
    st.counter += 1;
    st.stack.push(v.clone());
    st.on_stack.insert(v.clone());
    for w in live_deps(graph, v) {
        let candidate = if !st.index.contains_key(w) {
            strongconnect(graph, w, st, out);
            st.low[w]
        } else if st.on_stack.contains(w) {
            st.index[w]
        } else {
            continue;
        };
        let lv = st.low.get_mut(v).unwrap();
        *lv = (*lv).min(candidate);
    }
    if st.low[v] == st.index[v] {
        let mut comp = Vec::new();
        while let Some(w) = st.stack.pop() {
            st.on_stack.remove(&w);
            let done = &w == v;
            comp.push(w);
            if done {
                break;
            }
        }
        comp.sort();
        out.push(comp);
    }
}

fn condense(graph: &Graph) -> Result<Condensed, Cycle> {
    let mut st = TarjanState::default();
    let mut comps = Vec::new();
    for id in graph.nodes.keys() {
        if !st.index.contains_key(id) {
            strongconnect(graph, id, &mut st, &mut comps);
        }
    }
    comps.sort();

    let recursive_ok = |c: &Vec<NodeId>| {
        c.iter()
            .all(|id| matches!(graph.nodes[id].kind, NodeKind::Method | NodeKind::Function))
    };
    let cyclic = |c: &Vec<NodeId>| c.len() > 1 || live_deps(graph, &c[0]).any(|d| d == &c[0]);
    if let Some(bad) = comps.iter().find(|c| cyclic(c) && !recursive_ok(c)) {
        return Err(Cycle(bad.clone()));
    }

    let of: BTreeMap<&NodeId, usize> = comps
        .iter()
        .enumerate()
        .flat_map(|(i, c)| c.iter().map(move |id| (id, i)))
        .collect();
    let deps = comps
        .iter()
        .enumerate()
        .map(|(i, c)| {
            c.iter()
                .flat_map(|id| live_deps(graph, id))
                .map(|d| of[d])
                .filter(|&j| j != i)
                .collect()
        })
        .collect();
    Ok(Condensed {
        members: comps,
        deps,
    })
}

fn component_order(c: &Condensed) -> Vec<usize> {
    let n = c.members.len();
    let mut remaining: Vec<usize> = c.deps.iter().map(BTreeSet::len).collect();
    let mut rev: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, ds) in c.deps.iter().enumerate() {
        for &d in ds {
            rev[d].push(i);
        }
    }
    let mut ready: BTreeSet<usize> = (0..n).filter(|&i| remaining[i] == 0).collect();
    let mut order = Vec::with_capacity(n);
    while let Some(i) = ready.pop_first() {
        order.push(i);
        for &r in &rev[i] {
            remaining[r] -= 1;
            if remaining[r] == 0 {
                ready.insert(r);
            }
        }
    }
    order
}

/// Dependencies before dependents; ties broken by `NodeId` for determinism.
/// Cycles are an error unless every member is a Method or Function (mutual
/// recursion), in which case members are emitted in `NodeId` order.
pub fn topological(graph: &Graph) -> Result<Vec<NodeId>, Cycle> {
    let c = condense(graph)?;
    Ok(component_order(&c)
        .into_iter()
        .flat_map(|i| c.members[i].iter().cloned())
        .collect())
}

/// Groups of nodes with no dependencies between groups' members at the same level,
/// suitable for parallel filling. Level 0 has no unfilled dependencies.
pub fn levels(graph: &Graph) -> Result<Vec<Vec<NodeId>>, Cycle> {
    let c = condense(graph)?;
    let mut level = vec![0usize; c.members.len()];
    let mut out: Vec<Vec<NodeId>> = Vec::new();
    for i in component_order(&c) {
        level[i] = c.deps[i].iter().map(|&d| level[d] + 1).max().unwrap_or(0);
        if out.len() <= level[i] {
            out.resize(level[i] + 1, Vec::new());
        }
        out[level[i]].extend(c.members[i].iter().cloned());
    }
    for l in &mut out {
        l.sort();
    }
    Ok(out)
}

/// Every node that transitively depends on `id`, excluding `id`.
pub fn dependents(graph: &Graph, id: &NodeId) -> Vec<NodeId> {
    let mut rev: BTreeMap<&NodeId, Vec<&NodeId>> = BTreeMap::new();
    for from in graph.nodes.keys() {
        for to in live_deps(graph, from) {
            rev.entry(to).or_default().push(from);
        }
    }
    let mut seen: BTreeSet<&NodeId> = BTreeSet::new();
    let mut stack = vec![id];
    while let Some(cur) = stack.pop() {
        for &p in rev.get(cur).into_iter().flatten() {
            if p != id && seen.insert(p) {
                stack.push(p);
            }
        }
    }
    seen.into_iter().cloned().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Node, Placement};

    fn nid(s: &str) -> NodeId {
        NodeId(s.to_string())
    }

    fn graph(nodes: &[(&str, NodeKind)], edges: &[(&str, &str)]) -> Graph {
        let mut g = Graph::default();
        for (n, k) in nodes {
            g.insert(Node {
                id: nid(n),
                kind: *k,
                signature: String::new(),
                body: None,
                impl_header: None,
                placement: Placement {
                    file: "a.rs".into(),
                    index: 0,
                },
            });
        }
        for (a, b) in edges {
            g.add_use(&nid(a), &nid(b));
        }
        g
    }

    fn ids(v: &[&str]) -> Vec<NodeId> {
        v.iter().map(|s| nid(s)).collect()
    }

    use NodeKind::{Function as F, Type as T};

    #[test]
    fn topological_orders_deps_first_with_id_ties() {
        let g = graph(
            &[("a", F), ("b", F), ("c", T), ("d", T)],
            &[("a", "c"), ("b", "c"), ("c", "d"), ("a", "missing")],
        );
        assert_eq!(topological(&g).unwrap(), ids(&["d", "c", "a", "b"]));
    }

    #[test]
    fn type_cycle_is_error() {
        let g = graph(&[("a", T), ("b", T)], &[("a", "b"), ("b", "a")]);
        assert_eq!(topological(&g).unwrap_err().0, ids(&["a", "b"]));
        assert!(levels(&g).is_err());
    }

    #[test]
    fn mixed_cycle_is_error() {
        let g = graph(&[("a", F), ("b", T)], &[("a", "b"), ("b", "a")]);
        assert!(topological(&g).is_err());
    }

    #[test]
    fn function_cycles_allowed() {
        let g = graph(
            &[("a", F), ("b", NodeKind::Method), ("c", T), ("z", F)],
            &[("b", "a"), ("a", "b"), ("a", "c"), ("z", "z"), ("z", "a")],
        );
        assert_eq!(topological(&g).unwrap(), ids(&["c", "a", "b", "z"]));
        assert_eq!(
            levels(&g).unwrap(),
            vec![ids(&["c"]), ids(&["a", "b"]), ids(&["z"])]
        );
    }

    #[test]
    fn self_loop_on_type_is_error() {
        let g = graph(&[("a", T)], &[("a", "a")]);
        assert!(topological(&g).is_err());
    }

    #[test]
    fn levels_group_independent_nodes() {
        let g = graph(
            &[("a", F), ("b", F), ("c", T), ("d", T)],
            &[("a", "c"), ("b", "d"), ("a", "b")],
        );
        assert_eq!(
            levels(&g).unwrap(),
            vec![ids(&["c", "d"]), ids(&["b"]), ids(&["a"])]
        );
    }

    #[test]
    fn dependents_are_transitive_and_exclude_self() {
        let g = graph(
            &[("a", F), ("b", F), ("c", T), ("d", T)],
            &[("a", "b"), ("b", "c"), ("c", "d"), ("d", "a")],
        );
        assert_eq!(dependents(&g, &nid("c")), ids(&["a", "b", "d"]));
        let g = graph(&[("a", F), ("b", F), ("c", T)], &[("a", "b"), ("b", "c")]);
        assert_eq!(dependents(&g, &nid("c")), ids(&["a", "b"]));
        assert!(dependents(&g, &nid("a")).is_empty());
    }
}
