//! Publication throttling and cross-instance handover in
//! `SessionBroadcaster`.

use super::super::*;
use super::support::*;
use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// `SessionBroadcaster::new` runs inside `load()`, before
/// `change_host_folder` rebuilds the WASI context and restarts
/// CLOCK_MONOTONIC. `startup_not_before` is therefore stamped in an epoch
/// that every later reading predates. `Instant` still advances, so this
/// delays rather than deadlocks -- but the delay is the stale offset,
/// which is unbounded from the broadcaster's point of view.
#[test]
fn publication_survives_a_clock_epoch_reset() {
    let started = Instant::now() + Duration::from_secs(3600);
    let (clock, mut broadcaster) = broadcaster(started, Duration::ZERO);

    // The WASI context is replaced: the monotonic clock restarts near zero.
    let epoch_reset = Instant::now();
    clock.set(epoch_reset);

    assert_eq!(
        broadcaster.submit(values("first")),
        BroadcasterAction::Schedule(TICK),
        "a deadline an hour ahead must be pulled back to one interval"
    );
    clock.set(epoch_reset + TICK);
    assert_eq!(broadcaster.on_timer(), BroadcasterAction::Published);
    assert_eq!(broadcaster.sink().completed, 1);
}
/// A past instant read through a restarted clock looks future-dated, which
/// would suppress every later publication.
#[test]
fn epoch_reset_does_not_future_date_the_last_publication() {
    let started = Instant::now();
    let (clock, mut broadcaster) = broadcaster(started, Duration::ZERO);

    clock.set(started + TICK);
    assert_eq!(
        broadcaster.submit(values("first")),
        BroadcasterAction::Published
    );

    // last_completed now sits in the old epoch, ahead of the new clock.
    let epoch_reset = started - Duration::from_secs(3600);
    clock.set(epoch_reset);
    assert_eq!(
        broadcaster.submit(values("second")),
        BroadcasterAction::Schedule(TICK)
    );
    clock.set(epoch_reset + TICK);
    assert_eq!(broadcaster.on_timer(), BroadcasterAction::Published);
    assert_eq!(broadcaster.sink().completed, 2);
}
#[test]
fn cooldown_coalesces_the_latest_payload() {
    let now = Instant::now();
    let (clock, mut broadcaster) = broadcaster(now, Duration::ZERO);

    assert_eq!(
        broadcaster.submit(values("old")),
        BroadcasterAction::Schedule(TICK)
    );
    clock.set(now + Duration::from_secs(1));
    assert_eq!(broadcaster.submit(values("new")), BroadcasterAction::None);
    clock.set(now + TICK);
    assert_eq!(broadcaster.on_timer(), BroadcasterAction::Published);
    assert_eq!(broadcaster.on_timer(), BroadcasterAction::None);

    let pushes = &broadcaster.sink().pushes;
    assert_eq!(pushes.len(), 2);
    assert_eq!(pushes[0].1, "pipe_netspeed");
    assert_eq!(pushes[0].2, "net-new");
    assert_eq!(pushes[1].1, "pipe_uptime");
    assert_eq!(pushes[1].2, "load-new");
    assert_eq!(broadcaster.sink().completed, 1);
}
#[test]
fn lease_retry_retains_the_pending_payload() {
    let now = Instant::now();
    let clock = TestClock::new(now);
    let sink = RetryOnceSink {
        attempts: 0,
        published: Vec::new(),
    };
    let mut broadcaster = SessionBroadcaster::new(TICK, clock.clone(), sink);

    assert_eq!(
        broadcaster.submit(values("pending")),
        BroadcasterAction::Schedule(TICK)
    );
    clock.set(now + TICK);
    assert_eq!(
        broadcaster.on_timer(),
        BroadcasterAction::Schedule(Duration::from_millis(100))
    );
    assert_eq!(
        broadcaster.submit(values("latest")),
        BroadcasterAction::None
    );
    assert_eq!(broadcaster.sink().attempts, 1);
    clock.advance(Duration::from_millis(100));
    assert_eq!(broadcaster.on_timer(), BroadcasterAction::Published);
    assert_eq!(broadcaster.sink().attempts, 2);
    assert_eq!(broadcaster.sink().published, vec![values("latest")]);
}
#[test]
fn alternating_probe_latency_delays_instead_of_dropping() {
    let now = Instant::now();
    let (clock, mut broadcaster) = broadcaster(now, Duration::ZERO);

    clock.set(now + Duration::from_millis(1_900));
    assert_eq!(
        broadcaster.submit(values("slow-1")),
        BroadcasterAction::Schedule(Duration::from_millis(100))
    );
    clock.set(now + Duration::from_secs(2));
    assert_eq!(broadcaster.on_timer(), BroadcasterAction::Published);

    clock.set(now + Duration::from_millis(2_100));
    assert_eq!(
        broadcaster.submit(values("fast-1")),
        BroadcasterAction::Schedule(Duration::from_millis(1_900))
    );
    clock.set(now + Duration::from_secs(4));
    assert_eq!(broadcaster.on_timer(), BroadcasterAction::Published);

    clock.set(now + Duration::from_millis(5_900));
    assert_eq!(
        broadcaster.submit(values("slow-2")),
        BroadcasterAction::Schedule(Duration::from_millis(100))
    );
    clock.set(now + Duration::from_secs(6));
    assert_eq!(broadcaster.on_timer(), BroadcasterAction::Published);

    clock.set(now + Duration::from_millis(6_100));
    assert_eq!(
        broadcaster.submit(values("fast-2")),
        BroadcasterAction::Schedule(Duration::from_millis(1_900))
    );
    clock.set(now + Duration::from_secs(8));
    assert_eq!(broadcaster.on_timer(), BroadcasterAction::Published);

    let netspeed: Vec<_> = broadcaster
        .sink()
        .pushes
        .iter()
        .filter(|(_, widget, _)| widget == "pipe_netspeed")
        .map(|(at, _, text)| (*at, text.as_str()))
        .collect();
    assert_eq!(
        netspeed,
        vec![
            (now + Duration::from_secs(2), "net-slow-1"),
            (now + Duration::from_secs(4), "net-fast-1"),
            (now + Duration::from_secs(6), "net-slow-2"),
            (now + Duration::from_secs(8), "net-fast-2"),
        ]
    );
}
#[test]
fn handover_payloads_share_one_publication_clock() {
    let now = Instant::now();
    let (clock, mut broadcaster) = broadcaster(now, Duration::ZERO);

    clock.set(now + TICK);
    assert_eq!(
        broadcaster.submit(values("old-instance")),
        BroadcasterAction::Published
    );
    clock.set(now + Duration::from_millis(2_100));
    assert_eq!(
        broadcaster.submit(values("replacement")),
        BroadcasterAction::Schedule(Duration::from_millis(1_900))
    );
    clock.set(now + Duration::from_secs(4));
    assert_eq!(broadcaster.on_timer(), BroadcasterAction::Published);

    let publication_times: Vec<_> = broadcaster
        .sink()
        .pushes
        .iter()
        .filter(|(_, widget, _)| widget == "pipe_netspeed")
        .map(|(at, _, _)| *at)
        .collect();
    assert_eq!(
        publication_times,
        vec![now + Duration::from_secs(2), now + Duration::from_secs(4)]
    );
}
#[test]
fn completion_is_recorded_after_both_widget_pushes() {
    let now = Instant::now();
    let push_duration = Duration::from_millis(100);
    let (clock, mut broadcaster) = broadcaster(now, push_duration);

    clock.set(now + TICK);
    assert_eq!(
        broadcaster.submit(values("first")),
        BroadcasterAction::Published
    );
    clock.set(now + Duration::from_millis(4_100));
    assert_eq!(
        broadcaster.submit(values("second")),
        BroadcasterAction::Schedule(Duration::from_millis(100))
    );
    assert_eq!(broadcaster.sink().pushes.len(), 2);

    clock.set(now + Duration::from_millis(4_200));
    assert_eq!(broadcaster.on_timer(), BroadcasterAction::Published);
    assert_eq!(broadcaster.sink().pushes.len(), 4);
}
#[test]
fn replacement_observes_an_old_instances_late_publication() {
    let now = Instant::now();
    let (clock, mut broadcaster) = broadcaster(now, Duration::ZERO);

    assert_eq!(
        broadcaster.submit(values("replacement")),
        BroadcasterAction::Schedule(TICK)
    );
    clock.set(now + Duration::from_millis(1_900));
    assert_eq!(
        broadcaster.observe_external_publication(),
        BroadcasterAction::Schedule(TICK)
    );
    clock.set(now + TICK);
    assert_eq!(broadcaster.on_timer(), BroadcasterAction::None);
    clock.set(now + Duration::from_millis(3_900));
    assert_eq!(broadcaster.on_timer(), BroadcasterAction::Published);
}
#[test]
fn replacement_broadcaster_waits_a_local_interval_for_an_unfamiliar_token() {
    let directory = TestDirectory::new("replacement-broadcaster");
    let state_path = directory.state_path();
    let publications = Rc::new(Cell::new(0));

    let old_now = Instant::now() + Duration::from_secs(60 * 60);
    let old_clock = TestClock::new(old_now);
    let mut old = SessionBroadcaster::new(
        TICK,
        old_clock.clone(),
        SharedLeaseSink {
            lease: SharedPublicationLease::new(TICK, state_path.clone(), 11),
            publications: publications.clone(),
        },
    );
    assert_eq!(old.submit(values("old")), BroadcasterAction::Schedule(TICK));
    old_clock.advance(TICK);
    assert_eq!(old.on_timer(), BroadcasterAction::Schedule(TICK));
    assert_eq!(publications.get(), 0);
    old_clock.advance(TICK);
    assert_eq!(old.on_timer(), BroadcasterAction::Published);

    let replacement_now = Instant::now();
    let replacement_clock = TestClock::new(replacement_now);
    let mut replacement = SessionBroadcaster::new(
        TICK,
        replacement_clock.clone(),
        SharedLeaseSink {
            lease: SharedPublicationLease::new(TICK, state_path, 12),
            publications: publications.clone(),
        },
    );
    assert_eq!(
        replacement.submit(values("replacement")),
        BroadcasterAction::Schedule(TICK)
    );
    replacement_clock.advance(TICK);
    assert_eq!(replacement.on_timer(), BroadcasterAction::Schedule(TICK));
    assert_eq!(publications.get(), 1);

    assert_eq!(
        replacement.submit(values("latest")),
        BroadcasterAction::None
    );
    replacement_clock.advance(TICK);
    assert_eq!(replacement.on_timer(), BroadcasterAction::Published);
    assert_eq!(publications.get(), 2);
}
