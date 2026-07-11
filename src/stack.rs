use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;

use thiserror::Error;
use uuid::Uuid;

use crate::model::{Claim, ReleaseMarker};
use crate::store::{Store, StoreError};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackNode {
    pub claim: Claim,
    pub active: bool,
    pub release: Option<ReleaseMarker>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StackProblem {
    MissingTarget {
        issue: NonZeroU64,
    },
    DuplicateTarget {
        issue: NonZeroU64,
    },
    MissingParent {
        child_claim_id: Uuid,
        parent_issue: NonZeroU64,
        parent_claim_id: Uuid,
        replacement_claim_id: Option<Uuid>,
    },
    BaseCycle {
        cycle: Vec<Uuid>,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StackResolution {
    pub nodes: Vec<StackNode>,
    pub problem: Option<StackProblem>,
}

pub fn resolve_stack(
    store: &Store,
    active: &[Claim],
    target_issue: NonZeroU64,
) -> Result<StackResolution, StackError> {
    let targets = active
        .iter()
        .filter(|claim| claim.issue == target_issue)
        .collect::<Vec<_>>();
    let target = match targets.as_slice() {
        [] => {
            return Ok(StackResolution {
                nodes: Vec::new(),
                problem: Some(StackProblem::MissingTarget {
                    issue: target_issue,
                }),
            });
        }
        [target] => (*target).clone(),
        _ => {
            return Ok(StackResolution {
                nodes: Vec::new(),
                problem: Some(StackProblem::DuplicateTarget {
                    issue: target_issue,
                }),
            });
        }
    };
    let active_ids = active
        .iter()
        .map(|claim| claim.claim_id)
        .collect::<BTreeSet<_>>();
    let replacements = active
        .iter()
        .map(|claim| (claim.issue, claim.claim_id))
        .collect::<BTreeMap<_, _>>();
    let mut chain = Vec::<StackNode>::new();
    let mut positions = BTreeMap::<Uuid, usize>::new();
    let mut current = target;

    loop {
        positions.insert(current.claim_id, chain.len());
        let release = store
            .list_releases(current.issue)?
            .into_iter()
            .find(|release| release.claim_id == current.claim_id);
        chain.push(StackNode {
            active: active_ids.contains(&current.claim_id),
            claim: current.clone(),
            release,
        });
        let (Some(parent_issue), Some(parent_claim_id)) =
            (current.base_issue, current.base_claim_id)
        else {
            chain.reverse();
            return Ok(StackResolution {
                nodes: chain,
                problem: None,
            });
        };
        if let Some(position) = positions.get(&parent_claim_id).copied() {
            let mut cycle = chain[position..]
                .iter()
                .map(|node| node.claim.claim_id)
                .collect::<Vec<_>>();
            cycle.push(parent_claim_id);
            let cycle = normalize_uuid_cycle(cycle);
            return Ok(StackResolution {
                nodes: Vec::new(),
                problem: Some(StackProblem::BaseCycle { cycle }),
            });
        }
        let parent = store
            .list_claims(parent_issue)?
            .into_iter()
            .find(|claim| claim.claim_id == parent_claim_id);
        let Some(parent) = parent else {
            return Ok(StackResolution {
                nodes: chain.into_iter().rev().collect(),
                problem: Some(StackProblem::MissingParent {
                    child_claim_id: current.claim_id,
                    parent_issue,
                    parent_claim_id,
                    replacement_claim_id: replacements.get(&parent_issue).copied(),
                }),
            });
        };
        current = parent;
    }
}

fn normalize_uuid_cycle(mut cycle: Vec<Uuid>) -> Vec<Uuid> {
    cycle.pop();
    let position = cycle
        .iter()
        .enumerate()
        .min_by_key(|(_, claim_id)| **claim_id)
        .map_or(0, |(position, _)| position);
    cycle.rotate_left(position);
    cycle.push(cycle[0]);
    cycle
}

pub fn wait_for_cycles(active: &[Claim], roots: &[NonZeroU64]) -> Vec<Vec<NonZeroU64>> {
    let by_issue = active
        .iter()
        .map(|claim| (claim.issue, claim))
        .collect::<BTreeMap<_, _>>();
    let mut starts = if roots.is_empty() {
        by_issue.keys().copied().collect::<Vec<_>>()
    } else {
        roots.to_vec()
    };
    starts.sort();
    starts.dedup();
    let mut visited = BTreeSet::new();
    let mut cycles = BTreeSet::new();
    for start in starts {
        walk_wait_graph(start, &by_issue, &mut Vec::new(), &mut visited, &mut cycles);
    }
    cycles.into_iter().collect()
}

fn walk_wait_graph(
    issue: NonZeroU64,
    by_issue: &BTreeMap<NonZeroU64, &Claim>,
    path: &mut Vec<NonZeroU64>,
    visited: &mut BTreeSet<NonZeroU64>,
    cycles: &mut BTreeSet<Vec<NonZeroU64>>,
) {
    if let Some(position) = path.iter().position(|candidate| *candidate == issue) {
        let mut cycle = path[position..].to_vec();
        cycle.push(issue);
        cycles.insert(normalize_issue_cycle(cycle));
        return;
    }
    if visited.contains(&issue) {
        return;
    }
    let Some(claim) = by_issue.get(&issue) else {
        return;
    };
    path.push(issue);
    let mut dependencies = claim
        .wait_for
        .iter()
        .map(|reference| reference.issue)
        .collect::<Vec<_>>();
    dependencies.sort();
    dependencies.dedup();
    for dependency in dependencies {
        walk_wait_graph(dependency, by_issue, path, visited, cycles);
    }
    path.pop();
    visited.insert(issue);
}

fn normalize_issue_cycle(mut cycle: Vec<NonZeroU64>) -> Vec<NonZeroU64> {
    cycle.pop();
    let position = cycle
        .iter()
        .enumerate()
        .min_by_key(|(_, issue)| **issue)
        .map_or(0, |(position, _)| position);
    cycle.rotate_left(position);
    cycle.push(cycle[0]);
    cycle
}

#[derive(Debug, Error)]
pub enum StackError {
    #[error(transparent)]
    Store(#[from] StoreError),
}
