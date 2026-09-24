//! Dependency graphs for task plans.
//!
//! A plan is a DAG: each node lists the nodes it depends on. The scheduler asks
//! the graph which nodes are *ready* (every dependency done, not already
//! active), and asks which nodes are transitively affected when one fails.
//! Everything is ordered (`BTreeMap`/`BTreeSet`) so scheduling decisions are
//! deterministic and replayable.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Errors found while building or validating a dependency graph.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GraphError {
    #[error("`{node}` depends on unknown node `{dependency}`")]
    UnknownDependency { node: String, dependency: String },
    #[error("`{node}` depends on itself")]
    SelfDependency { node: String },
    #[error("dependency cycle: {}", .path.join(" -> "))]
    Cycle { path: Vec<String> },
}

/// A directed acyclic graph of nodes keyed by `K`, where an edge `a -> b`
/// means "`a` depends on `b`".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DepGraph<K: Ord + Clone> {
    deps: BTreeMap<K, BTreeSet<K>>,
}

impl<K: Ord + Clone> Default for DepGraph<K> {
    fn default() -> Self {
        Self {
            deps: BTreeMap::new(),
        }
    }
}

impl<K: Ord + Clone + std::fmt::Display> DepGraph<K> {
    pub fn new() -> Self {
        Self::default()
    }

    /// Builds and validates a graph from `(node, dependencies)` pairs.
    pub fn from_nodes<I, D>(nodes: I) -> Result<Self, GraphError>
    where
        I: IntoIterator<Item = (K, D)>,
        D: IntoIterator<Item = K>,
    {
        let mut graph = Self::new();
        let mut pending = Vec::new();
        for (node, deps) in nodes {
            graph.add_node(node.clone());
            pending.push((node, deps.into_iter().collect::<Vec<_>>()));
        }
        for (node, deps) in pending {
            for dep in deps {
                graph.add_dependency(&node, dep)?;
            }
        }
        graph.validate()?;
        Ok(graph)
    }

    /// Adds a node with no dependencies. Adding an existing node is a no-op.
    pub fn add_node(&mut self, node: K) {
        self.deps.entry(node).or_default();
    }

    /// Records that `node` depends on `dependency`. Both must already exist.
    pub fn add_dependency(&mut self, node: &K, dependency: K) -> Result<(), GraphError> {
        if *node == dependency {
            return Err(GraphError::SelfDependency {
                node: node.to_string(),
            });
        }
        if !self.deps.contains_key(&dependency) {
            return Err(GraphError::UnknownDependency {
                node: node.to_string(),
                dependency: dependency.to_string(),
            });
        }
        match self.deps.get_mut(node) {
            Some(set) => {
                set.insert(dependency);
                Ok(())
            }
            None => Err(GraphError::UnknownDependency {
                node: node.to_string(),
                dependency: dependency.to_string(),
            }),
        }
    }

    pub fn len(&self) -> usize {
        self.deps.len()
    }

    pub fn is_empty(&self) -> bool {
        self.deps.is_empty()
    }

    pub fn contains(&self, node: &K) -> bool {
        self.deps.contains_key(node)
    }

    pub fn nodes(&self) -> impl Iterator<Item = &K> {
        self.deps.keys()
    }

    /// The direct dependencies of `node`.
    pub fn dependencies(&self, node: &K) -> impl Iterator<Item = &K> {
        self.deps.get(node).into_iter().flatten()
    }

    /// Nodes that directly depend on `node`.
    pub fn dependents<'a>(&'a self, node: &'a K) -> impl Iterator<Item = &'a K> + 'a {
        self.deps
            .iter()
            .filter(move |(_, deps)| deps.contains(node))
            .map(|(n, _)| n)
    }

    /// Every node that directly or indirectly depends on `node`. Used to block
    /// or skip downstream work when a node fails for good.
    pub fn transitive_dependents(&self, node: &K) -> BTreeSet<K> {
        let mut out = BTreeSet::new();
        let mut queue = VecDeque::from([node.clone()]);
        while let Some(current) = queue.pop_front() {
            for dependent in self.dependents(&current) {
                if out.insert(dependent.clone()) {
                    queue.push_back(dependent.clone());
                }
            }
        }
        out
    }

    /// Checks the graph has no cycles.
    pub fn validate(&self) -> Result<(), GraphError> {
        self.topo_order().map(|_| ())
    }

    /// A deterministic topological order (dependencies first). Ties are broken
    /// by key order so the same plan always schedules the same way.
    pub fn topo_order(&self) -> Result<Vec<K>, GraphError> {
        let mut remaining: BTreeMap<&K, usize> =
            self.deps.iter().map(|(n, d)| (n, d.len())).collect();
        let mut ready: BTreeSet<&K> = remaining
            .iter()
            .filter(|(_, count)| **count == 0)
            .map(|(n, _)| *n)
            .collect();
        let mut order = Vec::with_capacity(self.deps.len());

        while let Some(node) = ready.pop_first() {
            remaining.remove(node);
            order.push(node.clone());
            for dependent in self.dependents(node) {
                if let Some(count) = remaining.get_mut(dependent) {
                    *count -= 1;
                    if *count == 0 {
                        ready.insert(dependent);
                    }
                }
            }
        }

        if remaining.is_empty() {
            Ok(order)
        } else {
            let stuck: BTreeSet<&K> = remaining.keys().copied().collect();
            Err(GraphError::Cycle {
                path: self.find_cycle(&stuck),
            })
        }
    }

    /// Nodes whose dependencies are all in `done` and that are neither done
    /// nor `active` themselves, in key order.
    pub fn ready(&self, done: &BTreeSet<K>, active: &BTreeSet<K>) -> Vec<K> {
        self.deps
            .iter()
            .filter(|(node, _)| !done.contains(*node) && !active.contains(*node))
            .filter(|(_, deps)| deps.iter().all(|d| done.contains(d)))
            .map(|(node, _)| node.clone())
            .collect()
    }

    /// Every node left over by Kahn's algorithm has at least one dependency
    /// that is also left over, so walking those edges must eventually repeat a
    /// node. The repeated stretch is a cycle; return it for the error message.
    fn find_cycle(&self, stuck: &BTreeSet<&K>) -> Vec<String> {
        let Some(start) = stuck.first() else {
            return Vec::new();
        };
        let mut path: Vec<&K> = vec![start];
        loop {
            let current = *path.last().expect("path is never empty");
            let next = self
                .dependencies(current)
                .find(|d| stuck.contains(d))
                .expect("a node left over by Kahn's algorithm has a left-over dependency");
            if let Some(pos) = path.iter().position(|n| *n == next) {
                let mut cycle: Vec<String> = path[pos..].iter().map(|n| n.to_string()).collect();
                cycle.push(next.to_string());
                return cycle;
            }
            path.push(next);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn plan(nodes: &[(&str, &[&str])]) -> Result<DepGraph<String>, GraphError> {
        DepGraph::from_nodes(
            nodes
                .iter()
                .map(|(n, deps)| (n.to_string(), deps.iter().map(|d| d.to_string()))),
        )
    }

    #[test]
    fn topo_order_is_deterministic_and_dependencies_first() {
        let g = plan(&[
            ("ui", &["api"]),
            ("api", &["schema"]),
            ("schema", &[]),
            ("docs", &[]),
        ])
        .unwrap();
        assert_eq!(g.topo_order().unwrap(), ["docs", "schema", "api", "ui"]);
    }

    #[test]
    fn ready_respects_done_and_active() {
        let g = plan(&[("a", &[]), ("b", &["a"]), ("c", &["a"]), ("d", &["b", "c"])]).unwrap();
        assert_eq!(g.ready(&set(&[]), &set(&[])), ["a"]);
        assert_eq!(g.ready(&set(&["a"]), &set(&["b"])), ["c"]);
        assert_eq!(
            g.ready(&set(&["a", "b"]), &set(&["c"])),
            Vec::<String>::new()
        );
        assert_eq!(g.ready(&set(&["a", "b", "c"]), &set(&[])), ["d"]);
    }

    #[test]
    fn transitive_dependents_follow_the_whole_chain() {
        let g = plan(&[("a", &[]), ("b", &["a"]), ("c", &["b"]), ("x", &[])]).unwrap();
        assert_eq!(g.transitive_dependents(&"a".to_string()), set(&["b", "c"]));
        assert!(g.transitive_dependents(&"x".to_string()).is_empty());
    }

    #[test]
    fn rejects_unknown_and_self_dependencies() {
        assert_eq!(
            plan(&[("a", &["ghost"])]).unwrap_err(),
            GraphError::UnknownDependency {
                node: "a".into(),
                dependency: "ghost".into()
            }
        );
        assert_eq!(
            plan(&[("a", &["a"])]).unwrap_err(),
            GraphError::SelfDependency { node: "a".into() }
        );
    }

    #[test]
    fn reports_the_cycle_even_when_scanning_starts_behind_it() {
        // "0" sorts first and sits two hops behind the loop b <-> c.
        let err = plan(&[("0", &["1"]), ("1", &["b"]), ("b", &["c"]), ("c", &["b"])]).unwrap_err();
        assert_eq!(
            err,
            GraphError::Cycle {
                path: vec!["b".into(), "c".into(), "b".into()]
            }
        );
    }

    #[test]
    fn reports_the_cycle() {
        let err = plan(&[("a", &["c"]), ("b", &["a"]), ("c", &["b"]), ("d", &["a"])]).unwrap_err();
        let GraphError::Cycle { path } = err else {
            panic!("expected a cycle error");
        };
        assert_eq!(path.first(), path.last());
        assert_eq!(
            path.len(),
            4,
            "a 3-node loop plus the repeated start: {path:?}"
        );
        for node in ["a", "b", "c"] {
            assert!(
                path.iter().any(|p| p == node),
                "{node} missing from {path:?}"
            );
        }
    }
}
