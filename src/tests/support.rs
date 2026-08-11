//! Shared fixtures for the unit tests.

use super::super::*;
use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub(super) const TICK: Duration = Duration::from_secs(2);

pub(super) fn ticker() -> SampleTicker {
    SampleTicker::new(TICK)
}

#[derive(Clone)]
pub(super) struct TestClock {
    now: Rc<Cell<Instant>>,
}

impl TestClock {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            now: Rc::new(Cell::new(now)),
        }
    }

    pub(super) fn set(&self, now: Instant) {
        self.now.set(now);
    }

    pub(super) fn advance(&self, duration: Duration) {
        self.now.set(self.now.get() + duration);
    }
}

impl Clock for TestClock {
    fn now(&self) -> Instant {
        self.now.get()
    }
}

pub(super) struct TestDirectory(pub(super) PathBuf);

impl TestDirectory {
    pub(super) fn new(label: &str) -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("zj-sysinfo-{label}-{}-{id}", std::process::id()));
        fs::create_dir(&path).expect("failed to create test directory");
        Self(path)
    }

    pub(super) fn state_path(&self) -> PathBuf {
        self.0.join("publication")
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

pub(super) struct RecordingSink {
    clock: TestClock,
    push_duration: Duration,
    pub(super) pushes: Vec<(Instant, String, String)>,
    pub(super) completed: usize,
}

impl RecordingSink {
    pub(super) fn new(clock: TestClock, push_duration: Duration) -> Self {
        Self {
            clock,
            push_duration,
            pushes: Vec::new(),
            completed: 0,
        }
    }
}

impl WidgetSink for RecordingSink {
    fn publish(&mut self, values: &WidgetValues) -> SinkAction {
        self.pushes.push((
            self.clock.now(),
            "pipe_netspeed".to_string(),
            values.netspeed.clone(),
        ));
        self.clock.advance(self.push_duration);
        self.pushes.push((
            self.clock.now(),
            "pipe_uptime".to_string(),
            values.uptime.clone(),
        ));
        self.clock.advance(self.push_duration);
        self.completed += 1;
        SinkAction::Published
    }
}

pub(super) struct RetryOnceSink {
    pub(super) attempts: usize,
    pub(super) published: Vec<WidgetValues>,
}

impl WidgetSink for RetryOnceSink {
    fn publish(&mut self, values: &WidgetValues) -> SinkAction {
        self.attempts += 1;
        if self.attempts == 1 {
            SinkAction::Retry(Duration::from_millis(100))
        } else {
            self.published.push(values.clone());
            SinkAction::Published
        }
    }
}

pub(super) struct SharedLeaseSink {
    pub(super) lease: SharedPublicationLease,
    pub(super) publications: Rc<Cell<usize>>,
}

impl WidgetSink for SharedLeaseSink {
    fn publish(&mut self, _values: &WidgetValues) -> SinkAction {
        let publications = self.publications.clone();
        self.lease
            .publish(|| publications.set(publications.get() + 1))
    }
}

pub(super) fn broadcaster(
    now: Instant,
    push_duration: Duration,
) -> (TestClock, SessionBroadcaster<TestClock, RecordingSink>) {
    let clock = TestClock::new(now);
    let sink = RecordingSink::new(clock.clone(), push_duration);
    let broadcaster = SessionBroadcaster::new(TICK, clock.clone(), sink);
    (clock, broadcaster)
}

pub(super) fn values(label: &str) -> WidgetValues {
    WidgetValues::new(format!("net-{label}"), format!("load-{label}"))
}
