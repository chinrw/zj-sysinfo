//! Sampling cadence: the clock-free timer accounting in `SampleTicker`
//! and `RetryTimer`.

use super::super::*;
use super::support::*;
use std::time::Duration;

#[test]
#[should_panic(expected = "ticker interval must be non-zero")]
fn ticker_rejects_zero_interval() {
    SampleTicker::new(Duration::ZERO);
}
#[test]
fn duplicate_timer_events_do_not_fork_the_tick_loop() {
    let mut ticker = ticker();

    assert_eq!(ticker.start(), Some(Duration::ZERO));
    assert_eq!(ticker.on_timer(), TimerAction::RunCycle);
    // Second event lands mid-cycle: no nested cycle, and the completion
    // below still owes a replacement timer.
    assert_eq!(ticker.on_timer(), TimerAction::Ignore);
    assert_eq!(ticker.on_cycle_completed(), Some(TICK));
    assert_eq!(ticker.on_timer(), TimerAction::RunCycle);
}
#[test]
fn retry_timer_fires_once_per_arming() {
    let mut retry = RetryTimer::new(TICK);

    assert!(!retry.on_timer(), "unsolicited event must not fire");
    assert_eq!(retry.arm(), TICK);
    assert!(retry.on_timer());
    assert!(!retry.on_timer(), "duplicate event must not fire");

    assert_eq!(retry.arm(), TICK);
    assert!(retry.on_timer());
}
#[test]
fn completed_cycle_asks_for_a_full_interval() {
    let mut ticker = ticker();

    ticker.start();
    assert_eq!(ticker.on_timer(), TimerAction::RunCycle);
    // The next deadline starts when the work finished, so however long the
    // cycle took cannot eat into the following interval.
    assert_eq!(ticker.on_cycle_completed(), Some(TICK));
}
#[test]
fn concurrent_schedules_collapse_into_one_cycle() {
    let mut ticker = ticker();

    ticker.start();
    // Publication retry and probe retry each arm their own timer.
    ticker.note_schedule();
    ticker.note_schedule();

    assert_eq!(ticker.on_timer(), TimerAction::RunCycle);
    assert_eq!(
        ticker.on_cycle_completed(),
        None,
        "two timers are still outstanding; arming a third would compound"
    );
    assert_eq!(ticker.on_timer(), TimerAction::RunCycle);
    assert_eq!(ticker.on_cycle_completed(), None);
    assert_eq!(ticker.on_timer(), TimerAction::RunCycle);
    assert_eq!(
        ticker.on_cycle_completed(),
        Some(TICK),
        "last outstanding timer consumed, so the loop must re-arm"
    );
}
/// The 2026-08-10 deadlock: `change_host_folder` rebuilds the WASI context
/// and restarts CLOCK_MONOTONIC, the deadline comparison answered Ignore
/// forever, and the Ignore path armed no replacement. `set_timeout` is
/// one-shot, so the plugin went silent for the whole session.
///
/// The ticker reads no clock now, so the epoch reset is unrepresentable
/// here; what this pins down is the property that made it fatal.
#[test]
fn ticker_never_strands_itself_without_a_pending_timer() {
    let mut idle = ticker();
    assert!(!idle.is_deadlocked());
    idle.start();
    assert!(!idle.is_deadlocked());

    // Every reachable interleaving of the three inputs, to a depth that
    // covers each phase transition more than once.
    for step in 0..3usize.pow(6) {
        let mut ticker = ticker();
        ticker.start();
        let mut code = step;
        for _ in 0..6 {
            match code % 3 {
                0 => {
                    if ticker.on_timer() == TimerAction::RunCycle {
                        if let Some(delay) = ticker.on_cycle_completed() {
                            assert_eq!(delay, TICK);
                        }
                    } else if let Some(delay) = ticker.ensure_armed() {
                        assert_eq!(delay, TICK);
                    }
                }
                1 => ticker.note_schedule(),
                _ => {
                    if let Some(delay) = ticker.ensure_armed() {
                        assert_eq!(delay, TICK);
                    }
                }
            }
            code /= 3;
            assert!(
                !ticker.is_deadlocked(),
                "armed with no outstanding timer at step {step}: {ticker:?}"
            );
        }
    }
}
