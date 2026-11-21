// Copyright 2026 Manu P Anand
// Licensed under the Apache License, Version 2.0
// See LICENSE for details.

//! Job dependency graphs: cycle detection and deterministic topological order.

use std::collections::BTreeSet;

/// A problem with a job dependency graph.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GraphError {
    /// A job needs a job that does not exist.
    #[error("job `{job}` needs unknown job `{need}`")]
    UnknownNeed {
        /// The job with the bad `needs` entry.
        job: String,
        /// The missing job.
        need: String,
    },
    /// The jobs form a cycle (first and last element are the same job).
    #[error("dependency cycle: {}", .0.join(" -> "))]
    Cycle(Vec<String>),
}

/// Find cycles with a depth-first search in declaration order. Each returned cycle lists
/// node indices and ends with its first node. At most `limit` cycles are returned.
pub(crate) fn find_cycles(deps: &[Vec<usize>], limit: usize) -> Vec<Vec<usize>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Color {
        White,
        Grey,
        Black,
    }
    let n = deps.len();
    let mut color = vec![Color::White; n];
    let mut cycles = Vec::new();
    for root in 0..n {
        if color[root] != Color::White {
            continue;
        }
        // Iterative DFS: stack of (node, next edge index); `path` mirrors grey nodes.
        let mut stack: Vec<(usize, usize)> = vec![(root, 0)];
        let mut path: Vec<usize> = vec![root];
        color[root] = Color::Grey;
        while let Some(top) = stack.last_mut() {
            let (node, edge) = *top;
            top.1 += 1;
            if let Some(&next) = deps[node].get(edge) {
                if next >= n {
                    continue;
                }
                match color[next] {
                    Color::White => {
                        color[next] = Color::Grey;
                        stack.push((next, 0));
                        path.push(next);
                    }
                    Color::Grey => {
                        if cycles.len() < limit
                            && let Some(pos) = path.iter().position(|&p| p == next)
                        {
                            let mut cycle = path[pos..].to_vec();
                            cycle.push(next);
                            cycles.push(cycle);
                        }
                    }
                    Color::Black => {}
                }
            } else {
                color[node] = Color::Black;
                stack.pop();
                path.pop();
            }
        }
    }
    cycles
}

/// Kahn's algorithm; among ready nodes the lowest index (declaration order) goes first.
/// `deps[i]` lists the nodes `i` depends on. Returns the order or a cycle.
pub(crate) fn topo_order(deps: &[Vec<usize>]) -> Result<Vec<usize>, Vec<usize>> {
    let n = deps.len();
    let mut remaining: Vec<usize> = vec![0; n];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, ds) in deps.iter().enumerate() {
        let unique: BTreeSet<usize> = ds.iter().copied().filter(|&d| d < n).collect();
        remaining[i] = unique.len();
        for d in unique {
            dependents[d].push(i);
        }
    }
    let mut ready: BTreeSet<usize> = (0..n).filter(|&i| remaining[i] == 0).collect();
    let mut order = Vec::with_capacity(n);
    while let Some(i) = ready.pop_first() {
        order.push(i);
        for &j in &dependents[i] {
            remaining[j] -= 1;
            if remaining[j] == 0 {
                ready.insert(j);
            }
        }
    }
    if order.len() == n {
        Ok(order)
    } else {
        Err(find_cycles(deps, 1).into_iter().next().unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn order_is_deterministic_by_declaration() {
        // 0: none, 1: [3], 2: [0], 3: [0]
        let deps = vec![vec![], vec![3], vec![0], vec![0]];
        assert_eq!(topo_order(&deps), Ok(vec![0, 2, 3, 1]));
    }

    #[test]
    fn cycles_are_reported_with_path() {
        let deps = vec![vec![1], vec![2], vec![0], vec![3]];
        let cycles = find_cycles(&deps, 10);
        assert_eq!(cycles, vec![vec![0, 1, 2, 0], vec![3, 3]]);
        assert!(topo_order(&deps).is_err());
    }

    #[test]
    fn display() {
        let e = GraphError::Cycle(vec!["a".into(), "b".into(), "a".into()]);
        assert_eq!(e.to_string(), "dependency cycle: a -> b -> a");
    }
}
