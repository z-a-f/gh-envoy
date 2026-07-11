use chrono::{TimeZone, Utc};
use gh_envoy::model::{Claim, ReleaseMarker, ReleaseReason, SCHEMA_VERSION, WaitForRef};
use gh_envoy::stack::{StackProblem, resolve_stack, wait_for_cycles};
use gh_envoy::store::Store;
use std::num::NonZeroU64;
use tempfile::TempDir;
use uuid::Uuid;

#[test]
fn released_exact_parent_is_used_instead_of_active_replacement() {
    let fixture = Fixture::new();
    let old_parent = claim(10, uuid(1), None);
    let replacement = claim(10, uuid(2), None);
    let child = claim(11, uuid(3), Some(&old_parent));
    for claim in [&old_parent, &replacement, &child] {
        fixture.persist_claim(claim);
    }
    fixture.persist_release(&old_parent, ReleaseReason::Merged);

    let resolution = resolve_stack(
        &fixture.store,
        &[replacement.clone(), child.clone()],
        child.issue,
    )
    .expect("resolve stack");

    assert_eq!(resolution.problem, None);
    assert_eq!(
        resolution
            .nodes
            .iter()
            .map(|node| node.claim.claim_id)
            .collect::<Vec<_>>(),
        [old_parent.claim_id, child.claim_id]
    );
    assert!(!resolution.nodes[0].active);
    assert_eq!(
        resolution.nodes[0]
            .release
            .as_ref()
            .expect("release")
            .reason,
        ReleaseReason::Merged
    );
    assert!(
        !resolution
            .nodes
            .iter()
            .any(|node| node.claim.claim_id == replacement.claim_id)
    );
}

#[test]
fn exact_base_cycle_has_no_false_root_order() {
    let fixture = Fixture::new();
    let mut first = claim(20, uuid(4), None);
    let mut second = claim(21, uuid(5), None);
    first.base_issue = Some(second.issue);
    first.base_claim_id = Some(second.claim_id);
    second.base_issue = Some(first.issue);
    second.base_claim_id = Some(first.claim_id);
    fixture.persist_claim(&first);
    fixture.persist_claim(&second);

    let resolution = resolve_stack(
        &fixture.store,
        &[first.clone(), second.clone()],
        first.issue,
    )
    .expect("resolve stack");

    assert!(resolution.nodes.is_empty());
    assert_eq!(
        resolution.problem,
        Some(StackProblem::BaseCycle {
            cycle: vec![first.claim_id, second.claim_id, first.claim_id],
        })
    );
}

#[test]
fn wait_for_cycles_are_detected_independently_by_issue() {
    let mut first = claim(30, uuid(6), None);
    let mut second = claim(31, uuid(7), None);
    first.wait_for.push(WaitForRef {
        issue: second.issue,
        claim_id: None,
    });
    second.wait_for.push(WaitForRef {
        issue: first.issue,
        claim_id: Some(first.claim_id),
    });

    assert_eq!(
        wait_for_cycles(&[first.clone(), second.clone()], &[first.issue]),
        vec![vec![first.issue, second.issue, first.issue]]
    );
    assert_eq!(
        resolve_stack(&Fixture::new().store, &[first.clone()], first.issue)
            .expect("base graph remains valid")
            .problem,
        None
    );
}

struct Fixture {
    _root: TempDir,
    store: Store,
}

impl Fixture {
    fn new() -> Self {
        let root = TempDir::new().expect("temporary store");
        let store = Store::new(root.path().join("envoy"));
        Self { _root: root, store }
    }

    fn persist_claim(&self, claim: &Claim) {
        self.store
            .lock()
            .expect("lock store")
            .create_claim(claim)
            .expect("create claim");
    }

    fn persist_release(&self, claim: &Claim, reason: ReleaseReason) {
        self.store
            .lock()
            .expect("lock store")
            .create_release(&ReleaseMarker {
                schema_version: SCHEMA_VERSION.to_owned(),
                repo: claim.repo.clone(),
                issue: claim.issue,
                claim_id: claim.claim_id,
                reason,
                released_at: Utc.with_ymd_and_hms(2026, 7, 10, 18, 0, 0).unwrap(),
            })
            .expect("create release");
    }
}

fn claim(number: u64, claim_id: Uuid, parent: Option<&Claim>) -> Claim {
    Claim {
        schema_version: SCHEMA_VERSION.to_owned(),
        claim_id,
        repo: "local/fixture".to_owned(),
        issue: issue(number),
        title: None,
        branch: format!("branch-{number}"),
        worktree: std::env::temp_dir().join(format!("worktree-{claim_id}")),
        base_remote: "origin".to_owned(),
        base_ref: parent.map_or_else(|| "main".to_owned(), |parent| parent.branch.clone()),
        base_sha: format!("sha-{number}"),
        base_issue: parent.map(|parent| parent.issue),
        base_claim_id: parent.map(|parent| parent.claim_id),
        wait_for: Vec::new(),
        declared_scope: None,
        note: None,
        created_at: Utc
            .with_ymd_and_hms(2026, 7, 10, 18, number as u32, 0)
            .unwrap(),
    }
}

fn issue(value: u64) -> NonZeroU64 {
    NonZeroU64::new(value).expect("positive issue")
}

fn uuid(value: u128) -> Uuid {
    Uuid::from_u128(value)
}
