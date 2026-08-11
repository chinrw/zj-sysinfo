//! The on-disk publication lease shared between plugin instances.

use super::super::*;
use super::support::*;
use std::cell::Cell;
use std::rc::Rc;

#[test]
fn shared_lease_waits_after_first_observing_missing_state() {
    let directory = TestDirectory::new("missing-shared-state");
    let mut lease = SharedPublicationLease::new(TICK, directory.state_path(), 11);
    let publications = Cell::new(0);

    assert_eq!(
        lease.publish(|| publications.set(publications.get() + 1)),
        SinkAction::Retry(TICK)
    );
    assert_eq!(publications.get(), 0);
    assert_eq!(
        lease.publish(|| publications.set(publications.get() + 1)),
        SinkAction::Published
    );
    assert_eq!(publications.get(), 1);
}
#[test]
fn shared_lease_fences_replacement_publications_with_tokens() {
    let directory = TestDirectory::new("shared-lease");
    let state_path = directory.state_path();
    let mut first = SharedPublicationLease::new(TICK, state_path.clone(), 11);
    let mut replacement = SharedPublicationLease::new(TICK, state_path.clone(), 12);
    let publications = Rc::new(Cell::new(0));

    let first_publications = publications.clone();
    assert_eq!(
        first.publish(|| first_publications.set(first_publications.get() + 1)),
        SinkAction::Retry(TICK)
    );
    assert_eq!(publications.get(), 0);
    assert_eq!(
        first.publish(|| first_publications.set(first_publications.get() + 1)),
        SinkAction::Published
    );
    assert_eq!(
        fs::read_to_string(&state_path).expect("failed to read first token"),
        "11:1"
    );

    let replacement_publications = publications.clone();
    assert_eq!(
        replacement
            .publish(|| { replacement_publications.set(replacement_publications.get() + 1) }),
        SinkAction::Retry(TICK)
    );
    assert_eq!(publications.get(), 1);

    assert_eq!(
        replacement.publish(|| publications.set(publications.get() + 1)),
        SinkAction::Published
    );
    assert_eq!(publications.get(), 2);
    assert_eq!(
        fs::read_to_string(&state_path).expect("failed to read replacement token"),
        "12:1"
    );

    assert_eq!(
        first.publish(|| publications.set(publications.get() + 1)),
        SinkAction::Retry(TICK)
    );
    assert_eq!(publications.get(), 2);
}
#[test]
fn shared_lease_recovers_an_abandoned_lock_before_publishing() {
    let directory = TestDirectory::new("held-lease");
    let state_path = directory.state_path();
    let lock_path = state_path.with_extension("lock");
    fs::create_dir(&lock_path).expect("failed to hold test lock");
    let mut lease = SharedPublicationLease::new(TICK, state_path.clone(), 7);
    let published = Cell::new(false);

    assert_eq!(
        lease.publish(|| published.set(true)),
        SinkAction::Retry(TICK)
    );
    assert!(!published.get());
    assert!(!lock_path.exists());
    assert_eq!(
        fs::read_to_string(&state_path).expect("failed to read repaired token"),
        "7:1"
    );

    assert_eq!(lease.publish(|| published.set(true)), SinkAction::Published);
    assert!(published.get());
}
#[test]
fn shared_lease_repairs_legacy_or_partial_state_before_publishing() {
    for (label, stored, nonce) in [
        ("legacy-state", "1750000000000000000", 9),
        ("partial-state", "partial", 10),
    ] {
        let directory = TestDirectory::new(label);
        let state_path = directory.state_path();
        fs::write(&state_path, stored).expect("failed to write invalid state");
        let mut lease = SharedPublicationLease::new(TICK, state_path.clone(), nonce);
        let published = Cell::new(false);

        assert_eq!(
            lease.publish(|| published.set(true)),
            SinkAction::Retry(TICK)
        );
        assert!(!published.get());
        assert_eq!(
            fs::read_to_string(&state_path).expect("failed to read repaired token"),
            format!("{nonce}:1")
        );

        assert_eq!(lease.publish(|| published.set(true)), SinkAction::Published);
        assert!(published.get());
    }
}
