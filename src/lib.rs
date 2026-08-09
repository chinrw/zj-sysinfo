use std::collections::BTreeMap;
use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const ACTIVE_CLIENT_ID: u16 = 1;
pub const PROBE_CONTEXT_KEY: &str = "zj-sysinfo";
pub const PROBE_CONTEXT_VALUE: &str = "macos-sysinfo";
pub const PROBE_CONTEXT_NONCE_KEY: &str = "instance-nonce";
pub const PROBE_CONTEXT_GENERATION_KEY: &str = "generation";
pub const PUBLICATION_COMPLETE_MESSAGE: &str = "zj-sysinfo-publication-complete";

pub fn is_active_client(client_id: u16) -> bool {
    client_id == ACTIVE_CLIENT_ID
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerAction {
    Ignore,
    RunCycle,
}

#[derive(Debug)]
enum TickerPhase {
    Idle,
    TimerArmed(Instant),
    CyclePending,
}

#[derive(Debug)]
pub struct SampleTicker {
    interval: Duration,
    phase: TickerPhase,
}

impl SampleTicker {
    pub fn new(interval: Duration) -> Self {
        assert!(!interval.is_zero(), "ticker interval must be non-zero");
        Self {
            interval,
            phase: TickerPhase::Idle,
        }
    }

    pub fn start(&mut self, now: Instant) -> Option<Duration> {
        if !matches!(self.phase, TickerPhase::Idle) {
            return None;
        }
        self.phase = TickerPhase::TimerArmed(now);
        Some(Duration::ZERO)
    }

    pub fn on_timer(&mut self, now: Instant) -> TimerAction {
        match self.phase {
            TickerPhase::TimerArmed(due) if now >= due => {
                self.phase = TickerPhase::CyclePending;
                TimerAction::RunCycle
            }
            _ => TimerAction::Ignore,
        }
    }

    pub fn on_cycle_completed(&mut self, now: Instant) -> Duration {
        assert!(
            matches!(self.phase, TickerPhase::CyclePending),
            "no cycle is pending"
        );
        let delay = self.interval;
        self.arm(now, delay);
        delay
    }

    fn arm(&mut self, now: Instant, delay: Duration) {
        self.phase = TickerPhase::TimerArmed(now + delay);
    }
}

#[derive(Debug)]
pub struct RetryTimer {
    interval: Duration,
    deadline: Option<Instant>,
}

impl RetryTimer {
    pub fn new(interval: Duration) -> Self {
        assert!(!interval.is_zero(), "retry interval must be non-zero");
        Self {
            interval,
            deadline: None,
        }
    }

    pub fn arm(&mut self, now: Instant) -> Duration {
        self.deadline = Some(now + self.interval);
        self.interval
    }

    pub fn on_timer(&mut self, now: Instant) -> bool {
        match self.deadline {
            Some(deadline) if now >= deadline => {
                self.deadline = None;
                true
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WidgetValues {
    pub netspeed: String,
    pub uptime: String,
}

impl WidgetValues {
    pub fn new(netspeed: impl Into<String>, uptime: impl Into<String>) -> Self {
        Self {
            netspeed: netspeed.into(),
            uptime: uptime.into(),
        }
    }
}

pub trait Clock {
    fn now(&self) -> Instant;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Instant {
        Instant::now()
    }
}

pub trait WidgetSink {
    fn publish(&mut self, values: &WidgetValues) -> SinkAction;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SinkAction {
    Published,
    Retry(Duration),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BroadcasterAction {
    None,
    Schedule(Duration),
    Published,
}

pub struct SessionBroadcaster<C, S> {
    interval: Duration,
    clock: C,
    sink: S,
    startup_not_before: Instant,
    external_not_before: Option<Instant>,
    last_completed: Option<Instant>,
    sink_retry_not_before: Option<Instant>,
    pending: Option<WidgetValues>,
    timer_deadline: Option<Instant>,
}

impl<C, S> SessionBroadcaster<C, S>
where
    C: Clock,
    S: WidgetSink,
{
    pub fn new(interval: Duration, clock: C, sink: S) -> Self {
        assert!(!interval.is_zero(), "publication interval must be non-zero");
        let startup_not_before = clock.now() + interval;
        Self {
            interval,
            clock,
            sink,
            startup_not_before,
            external_not_before: None,
            last_completed: None,
            sink_retry_not_before: None,
            pending: None,
            timer_deadline: None,
        }
    }

    pub fn submit(&mut self, values: WidgetValues) -> BroadcasterAction {
        self.pending = Some(values);
        self.flush_or_schedule()
    }

    pub fn on_timer(&mut self) -> BroadcasterAction {
        let Some(deadline) = self.timer_deadline else {
            return BroadcasterAction::None;
        };
        if self.clock.now() < deadline {
            return BroadcasterAction::None;
        }
        self.timer_deadline = None;
        self.flush_or_schedule()
    }

    pub fn observe_external_publication(&mut self) -> BroadcasterAction {
        let not_before = self.clock.now() + self.interval;
        self.external_not_before = Some(
            self.external_not_before
                .map_or(not_before, |current| current.max(not_before)),
        );
        self.flush_or_schedule()
    }

    #[cfg(test)]
    fn sink(&self) -> &S {
        &self.sink
    }

    fn flush_or_schedule(&mut self) -> BroadcasterAction {
        if self.pending.is_none() {
            return BroadcasterAction::None;
        }

        let now = self.clock.now();
        let due = self.next_due();
        if now < due {
            if self.timer_deadline == Some(due) {
                return BroadcasterAction::None;
            }
            self.timer_deadline = Some(due);
            return BroadcasterAction::Schedule(due.saturating_duration_since(now));
        }

        self.timer_deadline = None;
        let values = self.pending.take().expect("pending values disappeared");
        match self.sink.publish(&values) {
            SinkAction::Published => {
                self.last_completed = Some(self.clock.now());
                self.sink_retry_not_before = None;
                BroadcasterAction::Published
            }
            SinkAction::Retry(delay) => {
                assert!(!delay.is_zero(), "publication retry must be non-zero");
                self.pending = Some(values);
                let now = self.clock.now();
                let retry_not_before = now + delay;
                self.sink_retry_not_before = Some(
                    self.sink_retry_not_before
                        .map_or(retry_not_before, |current| current.max(retry_not_before)),
                );
                let retry_not_before = self
                    .sink_retry_not_before
                    .expect("publication retry deadline disappeared");
                self.timer_deadline = Some(retry_not_before);
                BroadcasterAction::Schedule(retry_not_before.saturating_duration_since(now))
            }
        }
    }

    fn next_due(&self) -> Instant {
        let mut due = self.startup_not_before;
        if let Some(external_not_before) = self.external_not_before {
            due = due.max(external_not_before);
        }
        if let Some(last_completed) = self.last_completed {
            due = due.max(last_completed + self.interval);
        }
        if let Some(sink_retry_not_before) = self.sink_retry_not_before {
            due = due.max(sink_retry_not_before);
        }
        due
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeToken {
    pub instance_nonce: u128,
    pub generation: u64,
}

pub fn probe_context(token: ProbeToken) -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            PROBE_CONTEXT_KEY.to_string(),
            PROBE_CONTEXT_VALUE.to_string(),
        ),
        (
            PROBE_CONTEXT_NONCE_KEY.to_string(),
            token.instance_nonce.to_string(),
        ),
        (
            PROBE_CONTEXT_GENERATION_KEY.to_string(),
            token.generation.to_string(),
        ),
    ])
}

pub fn probe_token_from_context(context: &BTreeMap<String, String>) -> Option<ProbeToken> {
    (context.get(PROBE_CONTEXT_KEY).map(String::as_str) == Some(PROBE_CONTEXT_VALUE))
        .then_some(())?;
    let instance_nonce = context.get(PROBE_CONTEXT_NONCE_KEY)?.parse().ok()?;
    let generation = context.get(PROBE_CONTEXT_GENERATION_KEY)?.parse().ok()?;
    Some(ProbeToken {
        instance_nonce,
        generation,
    })
}

pub fn publication_completion_nonce(
    expected_plugin_id: u32,
    source_plugin_id: Option<u32>,
    is_private: bool,
    name: &str,
    payload: Option<&str>,
) -> Option<u128> {
    (is_private
        && source_plugin_id == Some(expected_plugin_id)
        && name == PUBLICATION_COMPLETE_MESSAGE)
        .then_some(())?;
    payload?.parse().ok()
}

pub fn instance_nonce_from_random(random: [u8; 16]) -> u128 {
    u128::from_le_bytes(random)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PublicationToken {
    instance_nonce: u128,
    generation: u64,
}

impl PublicationToken {
    fn parse(value: &str) -> Option<Self> {
        let (instance_nonce, generation) = value.trim().split_once(':')?;
        Some(Self {
            instance_nonce: instance_nonce.parse().ok()?,
            generation: generation.parse().ok()?,
        })
    }

    fn encode(self) -> String {
        format!("{}:{}", self.instance_nonce, self.generation)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ObservedPublication {
    Unobserved,
    Missing,
    Token(PublicationToken),
}

/// Cross-instance publication gate for Zellij's per-plugin FIFO executor.
///
/// Calls for one state path must be externally serialized. This lets a later
/// call treat an existing lock marker as abandoned rather than in flight.
pub struct SharedPublicationLease {
    interval: Duration,
    instance_nonce: u128,
    next_generation: u64,
    observed_publication: ObservedPublication,
    state_path: PathBuf,
    lock_path: PathBuf,
}

impl SharedPublicationLease {
    pub fn new(interval: Duration, state_path: PathBuf, instance_nonce: u128) -> Self {
        assert!(!interval.is_zero(), "lease interval must be non-zero");
        let lock_path = state_path.with_extension("lock");
        Self {
            interval,
            instance_nonce,
            next_generation: 0,
            observed_publication: ObservedPublication::Unobserved,
            state_path,
            lock_path,
        }
    }

    pub fn publish<F>(&mut self, publish: F) -> SinkAction
    where
        F: FnOnce(),
    {
        let mut lock = match fs::create_dir(&self.lock_path) {
            Ok(()) => LeaseLock::new(self.lock_path.clone()),
            Err(error) if error.kind() == ErrorKind::AlreadyExists => {
                return self.recover_abandoned_lock();
            }
            Err(_) => return SinkAction::Retry(self.interval),
        };
        match fs::read_to_string(&self.state_path) {
            Ok(value) => {
                let Some(token) = PublicationToken::parse(&value) else {
                    return self.repair_state(&mut lock);
                };
                if self.observed_publication != ObservedPublication::Token(token) {
                    self.observed_publication = ObservedPublication::Token(token);
                    lock.remove_on_drop = true;
                    return SinkAction::Retry(self.interval);
                }
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                if self.observed_publication != ObservedPublication::Missing {
                    self.observed_publication = ObservedPublication::Missing;
                    lock.remove_on_drop = true;
                    return SinkAction::Retry(self.interval);
                }
            }
            Err(_) => {
                lock.remove_on_drop = true;
                return SinkAction::Retry(self.interval);
            }
        }

        let token = self.next_token();
        publish();
        if self.write_token(token).is_ok() {
            self.observed_publication = ObservedPublication::Token(token);
            lock.remove_on_drop = true;
        }
        SinkAction::Published
    }

    fn recover_abandoned_lock(&mut self) -> SinkAction {
        // Zellij 0.44.3 executes every callback and replacement load for one
        // plugin_id on the same FIFO thread. Observing this marker therefore
        // proves its callback ended or trapped; it cannot still be publishing.
        let token = self.next_token();
        if self.write_token(token).is_err() {
            return SinkAction::Retry(self.interval);
        }
        self.observed_publication = ObservedPublication::Token(token);
        if remove_lock_path(&self.lock_path).is_err() {
            return SinkAction::Retry(self.interval);
        }
        SinkAction::Retry(self.interval)
    }

    fn repair_state(&mut self, lock: &mut LeaseLock) -> SinkAction {
        let token = self.next_token();
        if self.write_token(token).is_ok() {
            self.observed_publication = ObservedPublication::Token(token);
            lock.remove_on_drop = true;
        }
        SinkAction::Retry(self.interval)
    }

    fn next_token(&mut self) -> PublicationToken {
        self.next_generation = self.next_generation.wrapping_add(1);
        PublicationToken {
            instance_nonce: self.instance_nonce,
            generation: self.next_generation,
        }
    }

    fn write_token(&self, token: PublicationToken) -> io::Result<()> {
        atomic_replace(&self.state_path, &token.encode())
    }
}

struct LeaseLock {
    path: PathBuf,
    remove_on_drop: bool,
}

impl LeaseLock {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            remove_on_drop: false,
        }
    }
}

impl Drop for LeaseLock {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = remove_lock_path(&self.path);
        }
    }
}

fn remove_lock_path(path: &Path) -> io::Result<()> {
    match fs::remove_dir(path) {
        Err(error) if error.kind() == ErrorKind::NotADirectory => fs::remove_file(path),
        result => result,
    }
}

fn atomic_replace(path: &Path, contents: &str) -> io::Result<()> {
    let mut temporary_name = path
        .file_name()
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidInput, "state path has no file name"))?
        .to_os_string();
    temporary_name.push(".next");
    let temporary_path = path.with_file_name(temporary_name);
    fs::write(&temporary_path, contents)?;
    fs::rename(temporary_path, path)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeAction {
    Start(ProbeToken),
    Wait,
    Restart(ProbeToken),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProbePhase {
    Idle,
    InFlight { missed_cycles: u32 },
}

#[derive(Debug)]
pub struct AsyncProbe {
    instance_nonce: u128,
    generation: u64,
    phase: ProbePhase,
    initial_abandon_after: u32,
    abandon_after: u32,
    max_abandon_after: u32,
}

impl AsyncProbe {
    pub fn new(instance_nonce: u128, abandon_after: u32, max_abandon_after: u32) -> Self {
        assert!(abandon_after > 0, "probe abandonment must be non-zero");
        assert!(
            max_abandon_after >= abandon_after,
            "maximum probe abandonment must cover the initial threshold"
        );
        Self {
            instance_nonce,
            generation: 0,
            phase: ProbePhase::Idle,
            initial_abandon_after: abandon_after,
            abandon_after,
            max_abandon_after,
        }
    }

    pub fn on_cycle(&mut self) -> ProbeAction {
        match self.phase {
            ProbePhase::Idle => ProbeAction::Start(self.start()),
            ProbePhase::InFlight { missed_cycles } => {
                let missed_cycles = missed_cycles.saturating_add(1);
                if missed_cycles < self.abandon_after {
                    self.phase = ProbePhase::InFlight { missed_cycles };
                    ProbeAction::Wait
                } else {
                    self.abandon_after = self
                        .abandon_after
                        .saturating_mul(2)
                        .min(self.max_abandon_after);
                    ProbeAction::Restart(self.start())
                }
            }
        }
    }

    pub fn complete(&mut self, token: ProbeToken) -> bool {
        if !matches!(self.phase, ProbePhase::InFlight { .. }) || token != self.current_token() {
            return false;
        }
        self.phase = ProbePhase::Idle;
        self.abandon_after = self.initial_abandon_after;
        true
    }

    pub fn cancel(&mut self) {
        if !matches!(self.phase, ProbePhase::Idle) {
            self.generation = self.generation.wrapping_add(1);
            self.phase = ProbePhase::Idle;
        }
        self.abandon_after = self.initial_abandon_after;
    }

    fn start(&mut self) -> ProbeToken {
        self.generation = self.generation.wrapping_add(1);
        self.phase = ProbePhase::InFlight { missed_cycles: 0 };
        self.current_token()
    }

    fn current_token(&self) -> ProbeToken {
        ProbeToken {
            instance_nonce: self.instance_nonce,
            generation: self.generation,
        }
    }
}

/// First route whose Destination is 00000000 (the default route).
pub fn default_iface(route: &str) -> Option<String> {
    route
        .lines()
        .skip(1)
        .filter_map(|line| {
            let mut cols = line.split_whitespace();
            let iface = cols.next()?;
            let dest = cols.next()?;
            (dest == "00000000").then(|| iface.to_string())
        })
        .next()
}

/// First non-lo interface with nonzero rx bytes (spec: no-default-route fallback).
pub fn fallback_iface(dev: &str) -> Option<String> {
    dev.lines().skip(2).find_map(|line| {
        let (name, rest) = line.trim_start().split_once(':')?;
        if name == "lo" {
            return None;
        }
        let rx: u64 = rest.split_whitespace().next()?.parse().ok()?;
        (rx > 0).then(|| name.to_string())
    })
}

/// (rx_bytes, tx_bytes) for `iface` from /proc/net/dev contents.
pub fn iface_bytes(dev: &str, iface: &str) -> Option<(u64, u64)> {
    let prefix = format!("{iface}:");
    let line = dev
        .lines()
        .map(str::trim_start)
        .find(|l| l.starts_with(&prefix))?;
    let fields: Vec<&str> = line[prefix.len()..].split_whitespace().collect();
    // Receive: bytes packets errs drop fifo frame compressed multicast (8)
    // Transmit bytes is the 9th field.
    let rx = fields.first()?.parse().ok()?;
    let tx = fields.get(8)?.parse().ok()?;
    Some((rx, tx))
}

/// (load1, load5) from /proc/loadavg contents.
pub fn loadavg(contents: &str) -> Option<(String, String)> {
    let mut fields = contents.split_whitespace();
    let one = fields.next()?.to_string();
    let five = fields.next()?.to_string();
    Some((one, five))
}

/// (load1, load5) from macOS `sysctl -n vm.loadavg` output.
pub fn parse_sysctl_loadavg(contents: &str) -> Option<(String, String)> {
    let contents = contents.trim();
    let contents = contents.strip_prefix('{')?.strip_suffix('}')?;
    loadavg(contents)
}

/// Default-route interface from macOS `route -n get default` output.
pub fn macos_default_iface(route: &str) -> Option<String> {
    route.lines().find_map(|line| {
        let iface = line.trim().strip_prefix("interface:")?.trim();
        (!iface.is_empty()).then(|| iface.to_string())
    })
}

/// netstat renders interface names as `%-10.10s`, so anything longer arrives
/// truncated while `route` reports it in full.
const NETSTAT_NAME_WIDTH: usize = 10;

/// Whether a netstat row belongs to `iface`, allowing for that truncation.
fn name_matches(row_name: &str, iface: &str) -> bool {
    row_name == iface || iface.get(..NETSTAT_NAME_WIDTH) == Some(row_name)
}

/// (ibytes, obytes) for `iface` from macOS `netstat -ibn` output.
pub fn macos_iface_bytes(netstat: &str, iface: &str) -> Option<(u64, u64)> {
    netstat.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let raw_name = *fields.first()?;
        let name = raw_name.strip_suffix('*').unwrap_or(raw_name);
        if !name_matches(name, iface) || !fields.get(2)?.starts_with("<Link#") {
            return None;
        }
        let ibytes = fields.get(fields.len().checked_sub(5)?)?.parse().ok()?;
        let obytes = fields.get(fields.len().checked_sub(2)?)?.parse().ok()?;
        Some((ibytes, obytes))
    })
}

/// First non-lo0 macOS interface with nonzero received bytes.
pub fn macos_fallback_iface(netstat: &str) -> Option<String> {
    netstat.lines().find_map(|line| {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let raw_name = *fields.first()?;
        let name = raw_name.strip_suffix('*').unwrap_or(raw_name);
        if name == "lo0" || !fields.get(2)?.starts_with("<Link#") {
            return None;
        }
        let ibytes: u64 = fields.get(fields.len().checked_sub(5)?)?.parse().ok()?;
        (ibytes > 0).then(|| name.to_string())
    })
}

#[derive(Debug, PartialEq, Eq)]
pub struct MacosProbe {
    pub counters: Option<(String, (u64, u64))>,
    pub loadavg: Option<(String, String)>,
}

/// Split and parse the three sections emitted by the macOS host probe.
pub fn parse_macos_probe(stdout: &str) -> Option<MacosProbe> {
    let mut sections = vec![String::new()];
    for line in stdout.lines() {
        if line.strip_suffix('\r').unwrap_or(line) == "@@" {
            sections.push(String::new());
        } else if let Some(section) = sections.last_mut() {
            section.push_str(line);
            section.push('\n');
        }
    }

    let mut sections = sections.into_iter();
    let load_section = sections.next()?;
    let route_section = sections.next()?;
    let netstat_section = sections.next()?;
    if sections.next().is_some() {
        return None;
    }

    let loadavg = parse_sysctl_loadavg(&load_section);
    let iface =
        macos_default_iface(&route_section).or_else(|| macos_fallback_iface(&netstat_section));
    let counters = iface
        .and_then(|iface| macos_iface_bytes(&netstat_section, &iface).map(|bytes| (iface, bytes)));
    Some(MacosProbe { counters, loadavg })
}

/// Bytes/second between two counter samples. Wraps and zero intervals → 0.
pub fn rate(prev: u64, cur: u64, elapsed_secs: f64) -> f64 {
    if elapsed_secs <= 0.0 || cur < prev {
        return 0.0;
    }
    (cur - prev) as f64 / elapsed_secs
}

/// Human-readable speed, matching the old script's style.
pub fn format_speed(bytes_per_sec: f64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    if bytes_per_sec >= GB {
        format!("{:.1} GB/s", bytes_per_sec / GB)
    } else if bytes_per_sec >= MB {
        format!("{:.1} MB/s", bytes_per_sec / MB)
    } else if bytes_per_sec >= KB {
        format!("{:.0} KB/s", bytes_per_sec / KB)
    } else {
        format!("{:.0} B/s", bytes_per_sec)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::path::PathBuf;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    const TICK: Duration = Duration::from_secs(2);

    fn ticker() -> SampleTicker {
        SampleTicker::new(TICK)
    }

    #[derive(Clone)]
    struct TestClock {
        now: Rc<Cell<Instant>>,
    }

    impl TestClock {
        fn new(now: Instant) -> Self {
            Self {
                now: Rc::new(Cell::new(now)),
            }
        }

        fn set(&self, now: Instant) {
            self.now.set(now);
        }

        fn advance(&self, duration: Duration) {
            self.now.set(self.now.get() + duration);
        }
    }

    impl Clock for TestClock {
        fn now(&self) -> Instant {
            self.now.get()
        }
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(label: &str) -> Self {
            static NEXT_ID: AtomicU64 = AtomicU64::new(0);
            let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("zj-sysinfo-{label}-{}-{id}", std::process::id()));
            fs::create_dir(&path).expect("failed to create test directory");
            Self(path)
        }

        fn state_path(&self) -> PathBuf {
            self.0.join("publication")
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct RecordingSink {
        clock: TestClock,
        push_duration: Duration,
        pushes: Vec<(Instant, String, String)>,
        completed: usize,
    }

    impl RecordingSink {
        fn new(clock: TestClock, push_duration: Duration) -> Self {
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

    struct RetryOnceSink {
        attempts: usize,
        published: Vec<WidgetValues>,
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

    struct SharedLeaseSink {
        lease: SharedPublicationLease,
        publications: Rc<Cell<usize>>,
    }

    impl WidgetSink for SharedLeaseSink {
        fn publish(&mut self, _values: &WidgetValues) -> SinkAction {
            let publications = self.publications.clone();
            self.lease
                .publish(|| publications.set(publications.get() + 1))
        }
    }

    fn broadcaster(
        now: Instant,
        push_duration: Duration,
    ) -> (TestClock, SessionBroadcaster<TestClock, RecordingSink>) {
        let clock = TestClock::new(now);
        let sink = RecordingSink::new(clock.clone(), push_duration);
        let broadcaster = SessionBroadcaster::new(TICK, clock.clone(), sink);
        (clock, broadcaster)
    }

    fn values(label: &str) -> WidgetValues {
        WidgetValues::new(format!("net-{label}"), format!("load-{label}"))
    }

    #[test]
    #[should_panic(expected = "ticker interval must be non-zero")]
    fn ticker_rejects_zero_interval() {
        SampleTicker::new(Duration::ZERO);
    }

    #[test]
    fn duplicate_timer_events_do_not_fork_the_tick_loop() {
        let now = Instant::now();
        let mut ticker = ticker();

        assert_eq!(ticker.start(now), Some(Duration::ZERO));
        assert_eq!(ticker.on_timer(now), TimerAction::RunCycle);
        assert_eq!(ticker.on_timer(now), TimerAction::Ignore);
        assert_eq!(ticker.on_cycle_completed(now), TICK);
        assert_eq!(
            ticker.on_timer(now + Duration::from_secs(1)),
            TimerAction::Ignore
        );
        assert_eq!(ticker.on_timer(now + TICK), TimerAction::RunCycle);
    }

    #[test]
    fn retry_timer_ignores_early_and_duplicate_events() {
        let now = Instant::now();
        let mut retry = RetryTimer::new(TICK);

        assert_eq!(retry.arm(now), TICK);
        assert!(!retry.on_timer(now + Duration::from_secs(1)));
        assert!(retry.on_timer(now + TICK));
        assert!(!retry.on_timer(now + TICK));

        assert_eq!(retry.arm(now + TICK), TICK);
        assert!(!retry.on_timer(now + Duration::from_secs(3)));
        assert!(retry.on_timer(now + TICK + TICK));
    }

    #[test]
    fn ticker_anchors_next_cycle_after_current_work() {
        let now = Instant::now();
        let mut ticker = ticker();

        ticker.start(now);
        assert_eq!(ticker.on_timer(now), TimerAction::RunCycle);
        let completed = now + Duration::from_millis(400);
        assert_eq!(ticker.on_cycle_completed(completed), TICK);
        assert_eq!(ticker.on_timer(now + TICK), TimerAction::Ignore);
        assert_eq!(ticker.on_timer(completed + TICK), TimerAction::RunCycle);
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
    fn probe_restarts_with_backoff_and_rejects_stale_results() {
        let mut probe = AsyncProbe::new(11, 3, 6);
        let ProbeAction::Start(first) = probe.on_cycle() else {
            panic!("first cycle did not start a probe");
        };
        assert_eq!(probe.on_cycle(), ProbeAction::Wait);
        assert_eq!(probe.on_cycle(), ProbeAction::Wait);
        let ProbeAction::Restart(second) = probe.on_cycle() else {
            panic!("third missed cycle did not restart the probe");
        };
        assert_ne!(first, second);
        assert!(!probe.complete(first));
        assert!(probe.complete(second));

        let ProbeAction::Start(third) = probe.on_cycle() else {
            panic!("completion did not return the probe to idle");
        };
        assert_eq!(third.generation, second.generation + 1);
    }

    #[test]
    fn probe_token_includes_the_plugin_instance_nonce() {
        let mut current = AsyncProbe::new(11, 3, 6);
        let mut replacement = AsyncProbe::new(12, 3, 6);
        let ProbeAction::Start(current_token) = current.on_cycle() else {
            panic!("current instance did not start a probe");
        };
        let ProbeAction::Start(replacement_token) = replacement.on_cycle() else {
            panic!("replacement instance did not start a probe");
        };

        assert_eq!(current_token.generation, replacement_token.generation);
        assert!(!replacement.complete(current_token));
        assert!(replacement.complete(replacement_token));
    }

    #[test]
    fn client_slot_one_is_the_only_active_runtime() {
        assert!(is_active_client(1));
        assert!(!is_active_client(0));
        assert!(!is_active_client(2));
    }

    #[test]
    fn probe_context_round_trip_rejects_missing_or_wrong_metadata() {
        let token = ProbeToken {
            instance_nonce: 42,
            generation: 7,
        };
        let context = probe_context(token);
        assert_eq!(probe_token_from_context(&context), Some(token));

        for key in [
            PROBE_CONTEXT_KEY,
            PROBE_CONTEXT_NONCE_KEY,
            PROBE_CONTEXT_GENERATION_KEY,
        ] {
            let mut incomplete = context.clone();
            incomplete.remove(key);
            assert_eq!(probe_token_from_context(&incomplete), None);
        }

        let mut wrong_probe = context.clone();
        wrong_probe.insert(PROBE_CONTEXT_KEY.to_string(), "other".to_string());
        assert_eq!(probe_token_from_context(&wrong_probe), None);

        let mut malformed = context;
        malformed.insert(PROBE_CONTEXT_GENERATION_KEY.to_string(), "NaN".to_string());
        assert_eq!(probe_token_from_context(&malformed), None);
    }

    #[test]
    fn publication_completion_requires_a_private_message_from_this_plugin() {
        let parse = |source_plugin_id, is_private, name: &str, payload| {
            publication_completion_nonce(17, source_plugin_id, is_private, name, payload)
        };

        assert_eq!(
            parse(Some(17), true, PUBLICATION_COMPLETE_MESSAGE, Some("42")),
            Some(42)
        );
        assert_eq!(
            parse(Some(18), true, PUBLICATION_COMPLETE_MESSAGE, Some("42")),
            None
        );
        assert_eq!(
            parse(Some(17), false, PUBLICATION_COMPLETE_MESSAGE, Some("42")),
            None
        );
        assert_eq!(parse(Some(17), true, "other", Some("42")), None);
        assert_eq!(
            parse(Some(17), true, PUBLICATION_COMPLETE_MESSAGE, Some("NaN")),
            None
        );
    }

    #[test]
    fn random_nonce_preserves_all_entropy_bits() {
        let random = [
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
            0xee, 0xff,
        ];
        assert_eq!(
            instance_nonce_from_random(random),
            0xffeeddccbbaa99887766554433221100
        );
    }

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

    const ROUTE: &str = "\
Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT
ens18\t00000000\t0100000A\t0003\t0\t0\t100\t00000000\t0\t0\t0
ens18\t0000000A\t00000000\t0001\t0\t0\t100\t00FFFFFF\t0\t0\t0
docker0\t000011AC\t00000000\t0001\t0\t0\t0\t0000FFFF\t0\t0\t0";

    const DEV: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 1917077    9483    0    0    0     0          0         0  1917077    9483    0    0    0     0       0          0
 ens18: 337403844  436153    0    0    0     0          0      1287 24246047  156373    0    0    0     0       0          0";

    const LOADAVG: &str = "0.52 0.48 0.36 2/1876 123456";

    const DEV_ONLY_LO_TRAFFIC: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 1917077    9483    0    0    0     0          0         0  1917077    9483    0    0    0     0       0          0
 ens18:        0       0    0    0    0     0          0         0        0        0    0    0    0     0       0          0";

    const MACOS_ROUTE: &str = "\
   route to: default
destination: default
       mask: default
    gateway: 192.168.20.1
  interface: en0
      flags: <UP,GATEWAY,DONE,STATIC,PRCLONING,GLOBAL>";

    const MACOS_NETSTAT: &str = "\
Name       Mtu   Network       Address            Ipkts Ierrs     Ibytes    Opkts Oerrs     Obytes  Coll
lo0        16384 <Link#1>                      186774513     0 49065918657 186774513     0 49065918657     0
lo0        16384 127           127.0.0.1       186774513     - 49065918657 186774513     - 49065918657     -
lo0        16384 ::1/128     ::1               186774513     - 49065918657 186774513     - 49065918657     -
lo0        16384 fe80::1%lo0 fe80:1::1         186774513     - 49065918657 186774513     - 49065918657     -
gif0*      1280  <Link#2>                             0     0          0        0     0          0     0
en0        1500  <Link#14>   12:b9:24:5c:29:6c 82511778     0 75656778687 89364180     0 79224021508     0
en0        1500  fe80::3e:47 fe80:e::3e:471a:d 82511778     - 75656778687 89364180     - 79224021508     -
en0        1500  192.168.20    192.168.20.29   82511778     - 75656778687 89364180     - 79224021508     -
utun1      1380  <Link#19>                            0     0          0     5709     0    2812862     0
utun1      1380  fe80::3834: fe80:13::3834:bfc        0     -          0     5709     -    2812862     -";

    #[test]
    fn fallback_iface_picks_first_non_lo_with_traffic() {
        assert_eq!(fallback_iface(DEV).as_deref(), Some("ens18"));
    }

    #[test]
    fn fallback_iface_none_when_only_lo_has_traffic() {
        assert_eq!(fallback_iface(DEV_ONLY_LO_TRAFFIC), None);
    }

    #[test]
    fn default_iface_picks_zero_destination() {
        assert_eq!(default_iface(ROUTE).as_deref(), Some("ens18"));
    }

    #[test]
    fn default_iface_none_on_empty() {
        assert_eq!(default_iface(""), None);
    }

    #[test]
    fn iface_bytes_parses_rx_tx() {
        assert_eq!(iface_bytes(DEV, "ens18"), Some((337403844, 24246047)));
    }

    #[test]
    fn iface_bytes_none_for_missing_iface() {
        assert_eq!(iface_bytes(DEV, "eth9"), None);
    }

    #[test]
    fn loadavg_takes_first_two_fields() {
        assert_eq!(
            loadavg(LOADAVG),
            Some(("0.52".to_string(), "0.48".to_string()))
        );
    }

    #[test]
    fn parse_sysctl_loadavg_strips_braces_and_takes_first_two_fields() {
        assert_eq!(
            parse_sysctl_loadavg("{ 2.85 3.13 3.64 }"),
            Some(("2.85".to_string(), "3.13".to_string()))
        );
        assert_eq!(parse_sysctl_loadavg(""), None);
        assert_eq!(parse_sysctl_loadavg("malformed"), None);
    }

    #[test]
    fn macos_default_iface_reads_interface_line() {
        assert_eq!(macos_default_iface(MACOS_ROUTE).as_deref(), Some("en0"));
        assert_eq!(
            macos_default_iface("route to: default\ngateway: 1.2.3.4"),
            None
        );
    }

    #[test]
    fn macos_iface_bytes_reads_link_rows_from_the_end() {
        assert_eq!(
            macos_iface_bytes(MACOS_NETSTAT, "en0"),
            Some((75656778687, 79224021508))
        );
        assert_eq!(
            macos_iface_bytes(MACOS_NETSTAT, "lo0"),
            Some((49065918657, 49065918657))
        );
        assert_eq!(
            macos_iface_bytes(MACOS_NETSTAT, "utun1"),
            Some((0, 2812862))
        );
        assert_eq!(macos_iface_bytes(MACOS_NETSTAT, "gif0"), Some((0, 0)));
        assert_eq!(macos_iface_bytes(MACOS_NETSTAT, "eth9"), None);
        assert_eq!(macos_iface_bytes("", "en0"), None);
    }

    #[test]
    fn macos_iface_bytes_matches_names_netstat_truncated_to_ten_chars() {
        // `route` reports the full name, netstat prints `%-10.10s`.
        const LONG: &str = "\
Name       Mtu   Network       Address            Ipkts Ierrs     Ibytes    Opkts Oerrs     Obytes  Coll
bridge1234 1500  <Link#20>   12:b9:24:5c:29:6c      7     0        700       9     0        900     0";
        assert_eq!(macos_iface_bytes(LONG, "bridge12345"), Some((700, 900)));
        assert_eq!(macos_iface_bytes(LONG, "bridge1234"), Some((700, 900)));
        // A shorter name must not match a longer row by prefix.
        assert_eq!(macos_iface_bytes(LONG, "bridge"), None);
        assert_eq!(macos_iface_bytes(MACOS_NETSTAT, "en"), None);
    }

    #[test]
    fn macos_fallback_iface_picks_first_non_lo0_with_traffic() {
        assert_eq!(macos_fallback_iface(MACOS_NETSTAT).as_deref(), Some("en0"));
        assert_eq!(macos_fallback_iface(""), None);
    }

    #[test]
    fn parse_macos_probe_drives_all_parsers() {
        let stdout = format!("{{ 2.85 3.13 3.64 }}\n@@\n{MACOS_ROUTE}\n@@\n{MACOS_NETSTAT}\n");
        assert_eq!(
            parse_macos_probe(&stdout),
            Some(MacosProbe {
                counters: Some(("en0".to_string(), (75656778687, 79224021508))),
                loadavg: Some(("2.85".to_string(), "3.13".to_string())),
            })
        );
        assert_eq!(parse_macos_probe(""), None);
        assert_eq!(parse_macos_probe("{ 2.85 3.13 3.64 }\n@@\n"), None);

        let no_route =
            format!("{{ 2.85 3.13 3.64 }}\n@@\nroute to: default\n@@\n{MACOS_NETSTAT}\n");
        assert_eq!(
            parse_macos_probe(&no_route),
            Some(MacosProbe {
                counters: Some(("en0".to_string(), (75656778687, 79224021508))),
                loadavg: Some(("2.85".to_string(), "3.13".to_string())),
            })
        );
    }

    #[test]
    fn rate_computes_bytes_per_second() {
        assert_eq!(rate(1000, 3000, 2.0), 1000.0);
    }

    #[test]
    fn rate_counter_wrap_is_zero() {
        assert_eq!(rate(3000, 1000, 2.0), 0.0);
    }

    #[test]
    fn rate_zero_elapsed_is_zero() {
        assert_eq!(rate(1000, 3000, 0.0), 0.0);
    }

    #[test]
    fn format_speed_units() {
        assert_eq!(format_speed(12.0), "12 B/s");
        assert_eq!(format_speed(340.0 * 1024.0), "340 KB/s");
        assert_eq!(format_speed(1.2 * 1024.0 * 1024.0), "1.2 MB/s");
        assert_eq!(format_speed(2.5 * 1024.0 * 1024.0 * 1024.0), "2.5 GB/s");
    }
}
