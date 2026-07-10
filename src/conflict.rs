use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU64;

use globset::{GlobBuilder, GlobMatcher};
use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

use crate::model::Claim;
use crate::observation::ClaimObservation;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlapRelationship {
    Sibling,
    Unrelated,
    Ancestor,
    Descendant,
    Consolidation,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlapConfidence {
    Full,
    Untracked,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlapSeverity {
    Info,
    Warning,
    Blocking,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct DiffOverlap {
    pub with_issue: NonZeroU64,
    pub with_claim_id: Uuid,
    pub relationship: OverlapRelationship,
    pub shared_paths: Vec<String>,
    pub confidence: OverlapConfidence,
    pub severity: OverlapSeverity,
    pub labels: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeWarningReason {
    OutsideAllowedScope,
    InsideDisallowedScope,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ScopeWarning {
    pub path: String,
    pub reason: ScopeWarningReason,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct EvidenceGroup {
    confidence: OverlapConfidence,
    severity: OverlapSeverity,
    labels: Vec<String>,
}

struct RiskRule {
    matcher: GlobMatcher,
    label: String,
}

pub fn analyze_claims(
    claims: &mut [ClaimObservation],
    risk_paths: &BTreeMap<String, String>,
) -> Result<(), ConflictError> {
    let risk_rules = compile_risk_rules(risk_paths)?;
    let by_id = claims
        .iter()
        .map(|observed| (observed.claim.claim_id, &observed.claim))
        .collect::<BTreeMap<_, _>>();
    let mut analyses = Vec::with_capacity(claims.len());

    for (index, subject) in claims.iter().enumerate() {
        let scope_warnings = scope_warnings(subject)?;
        let mut overlaps = Vec::new();
        for (other_index, other) in claims.iter().enumerate() {
            if index == other_index {
                continue;
            }
            let (Some(subject_diff), Some(other_diff)) = (&subject.diff, &other.diff) else {
                continue;
            };
            let relationship = relationship(&subject.claim, &other.claim, &by_id);
            let subject_tracked = subject_diff
                .changed_paths
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            let subject_untracked = subject_diff
                .untracked_paths
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            let other_tracked = other_diff
                .changed_paths
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            let other_untracked = other_diff
                .untracked_paths
                .iter()
                .map(String::as_str)
                .collect::<BTreeSet<_>>();
            let full = subject_tracked
                .intersection(&other_tracked)
                .copied()
                .collect::<BTreeSet<_>>();
            let mut untracked = subject_untracked
                .intersection(&other_tracked)
                .chain(subject_untracked.intersection(&other_untracked))
                .chain(other_untracked.intersection(&subject_tracked))
                .copied()
                .collect::<BTreeSet<_>>();
            for path in &full {
                untracked.remove(path);
            }

            let mut groups = BTreeMap::<EvidenceGroup, Vec<String>>::new();
            add_evidence(
                &mut groups,
                full,
                OverlapConfidence::Full,
                relationship,
                &risk_rules,
            );
            add_evidence(
                &mut groups,
                untracked,
                OverlapConfidence::Untracked,
                relationship,
                &risk_rules,
            );
            overlaps.extend(groups.into_iter().map(|(group, shared_paths)| DiffOverlap {
                with_issue: other.claim.issue,
                with_claim_id: other.claim.claim_id,
                relationship,
                shared_paths,
                confidence: group.confidence,
                severity: group.severity,
                labels: group.labels,
            }));
        }
        overlaps.sort_by(|left, right| {
            left.with_issue
                .cmp(&right.with_issue)
                .then_with(|| left.with_claim_id.cmp(&right.with_claim_id))
                .then_with(|| left.confidence.cmp(&right.confidence))
                .then_with(|| left.severity.cmp(&right.severity))
                .then_with(|| left.labels.cmp(&right.labels))
                .then_with(|| left.shared_paths.cmp(&right.shared_paths))
        });
        analyses.push((overlaps, scope_warnings));
    }

    for (claim, (overlaps, scope_warnings)) in claims.iter_mut().zip(analyses) {
        claim.overlaps = overlaps;
        claim.scope_warnings = scope_warnings;
    }
    Ok(())
}

fn add_evidence(
    groups: &mut BTreeMap<EvidenceGroup, Vec<String>>,
    paths: BTreeSet<&str>,
    confidence: OverlapConfidence,
    relationship: OverlapRelationship,
    risk_rules: &[RiskRule],
) {
    for path in paths {
        let labels = risk_labels(path, risk_rules);
        let severity = severity(relationship, !labels.is_empty());
        groups
            .entry(EvidenceGroup {
                confidence,
                severity,
                labels,
            })
            .or_default()
            .push(path.to_owned());
    }
}

fn relationship(
    subject: &Claim,
    other: &Claim,
    by_id: &BTreeMap<Uuid, &Claim>,
) -> OverlapRelationship {
    if reaches_parent(subject, other.claim_id, by_id) {
        OverlapRelationship::Ancestor
    } else if reaches_parent(other, subject.claim_id, by_id) {
        OverlapRelationship::Descendant
    } else if subject
        .wait_for
        .iter()
        .any(|reference| reference.claim_id == Some(other.claim_id))
        || other
            .wait_for
            .iter()
            .any(|reference| reference.claim_id == Some(subject.claim_id))
    {
        OverlapRelationship::Consolidation
    } else if subject.base_claim_id.is_some() && subject.base_claim_id == other.base_claim_id {
        OverlapRelationship::Sibling
    } else {
        OverlapRelationship::Unrelated
    }
}

fn reaches_parent(start: &Claim, target: Uuid, by_id: &BTreeMap<Uuid, &Claim>) -> bool {
    let mut current = start.base_claim_id;
    let mut visited = BTreeSet::new();
    while let Some(claim_id) = current {
        if claim_id == target {
            return true;
        }
        if !visited.insert(claim_id) {
            return false;
        }
        current = by_id.get(&claim_id).and_then(|claim| claim.base_claim_id);
    }
    false
}

fn severity(relationship: OverlapRelationship, risk: bool) -> OverlapSeverity {
    match (relationship, risk) {
        (OverlapRelationship::Sibling | OverlapRelationship::Unrelated, false) => {
            OverlapSeverity::Warning
        }
        (OverlapRelationship::Sibling | OverlapRelationship::Unrelated, true) => {
            OverlapSeverity::Blocking
        }
        (_, false) => OverlapSeverity::Info,
        (_, true) => OverlapSeverity::Warning,
    }
}

fn scope_warnings(observed: &ClaimObservation) -> Result<Vec<ScopeWarning>, ConflictError> {
    let (Some(scope), Some(diff)) = (&observed.claim.declared_scope, &observed.diff) else {
        return Ok(Vec::new());
    };
    if scope.allowed_paths.is_empty() && scope.disallowed_paths.is_empty() {
        return Ok(Vec::new());
    }
    let allowed = compile_matchers(&scope.allowed_paths)?;
    let disallowed = compile_matchers(&scope.disallowed_paths)?;
    let paths = diff
        .changed_paths
        .iter()
        .chain(&diff.untracked_paths)
        .collect::<BTreeSet<_>>();
    let mut warnings = Vec::new();
    for path in paths {
        if !allowed.is_empty() && !allowed.iter().any(|matcher| matcher.is_match(path)) {
            warnings.push(ScopeWarning {
                path: path.clone(),
                reason: ScopeWarningReason::OutsideAllowedScope,
            });
        }
        if disallowed.iter().any(|matcher| matcher.is_match(path)) {
            warnings.push(ScopeWarning {
                path: path.clone(),
                reason: ScopeWarningReason::InsideDisallowedScope,
            });
        }
    }
    warnings.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.reason.cmp(&right.reason))
    });
    Ok(warnings)
}

fn compile_risk_rules(
    risk_paths: &BTreeMap<String, String>,
) -> Result<Vec<RiskRule>, ConflictError> {
    risk_paths
        .iter()
        .map(|(pattern, label)| {
            Ok(RiskRule {
                matcher: compile_glob(pattern)?,
                label: label.clone(),
            })
        })
        .collect()
}

fn compile_matchers(patterns: &[String]) -> Result<Vec<GlobMatcher>, ConflictError> {
    patterns
        .iter()
        .map(|pattern| compile_glob(pattern))
        .collect()
}

fn risk_labels(path: &str, rules: &[RiskRule]) -> Vec<String> {
    rules
        .iter()
        .filter(|rule| rule.matcher.is_match(path))
        .map(|rule| rule.label.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

pub fn validate_glob_pattern(pattern: &str) -> Result<(), ConflictError> {
    compile_glob(pattern).map(|_| ())
}

fn compile_glob(pattern: &str) -> Result<GlobMatcher, ConflictError> {
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .backslash_escape(false)
        .build()
        .map(|glob| glob.compile_matcher())
        .map_err(|source| ConflictError::InvalidGlob {
            pattern: pattern.to_owned(),
            source,
        })
}

#[derive(Debug, Error)]
pub enum ConflictError {
    #[error("invalid path glob {pattern:?}: {source}")]
    InvalidGlob {
        pattern: String,
        #[source]
        source: globset::Error,
    },
}
