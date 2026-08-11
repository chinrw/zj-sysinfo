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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TickerPhase {
    Idle,
    Armed,
    CyclePending,
}

/// Cadence driver for the sampling loop.
///
/// It never reads a clock. Zellij rebuilds the plugin's WASI context on
/// `change_host_folder`, which restarts CLOCK_MONOTONIC at zero, so an
/// `Instant` captured before the swap sits permanently in the future of the
/// clock that comes after it. Comparing a stored deadline against `now` made
/// `on_timer` answer `Ignore` forever; because `set_timeout` is one-shot and
/// the `Ignore` path armed no replacement, the plugin went silent for the rest
/// of the session with no way back.
///
/// Instead every `set_timeout` is counted. A timer event is by definition one
/// of ours coming due, so the cycle runs on arrival and a replacement is armed
/// only when no other scheduled timer is still outstanding, which is what
/// collapses redundant schedules.
///
/// Invariant: a non-idle ticker always has at least one outstanding timer.
/// Violating it is the deadlock described above.
#[derive(Debug)]
pub struct SampleTicker {
    interval: Duration,
    phase: TickerPhase,
    outstanding: u32,
}

impl SampleTicker {
    pub fn new(interval: Duration) -> Self {
        assert!(!interval.is_zero(), "ticker interval must be non-zero");
        Self {
            interval,
            phase: TickerPhase::Idle,
            outstanding: 0,
        }
    }

    pub fn start(&mut self) -> Option<Duration> {
        if !matches!(self.phase, TickerPhase::Idle) {
            return None;
        }
        self.phase = TickerPhase::Armed;
        self.outstanding += 1;
        Some(Duration::ZERO)
    }

    /// Record a `set_timeout` armed by something other than the ticker itself
    /// (probe retry, publication retry). Keeping one counter for every timer
    /// this plugin arms is what makes the outstanding count trustworthy.
    pub fn note_schedule(&mut self) {
        self.outstanding += 1;
    }

    pub fn on_timer(&mut self) -> TimerAction {
        self.outstanding = self.outstanding.saturating_sub(1);
        match self.phase {
            TickerPhase::Armed => {
                self.phase = TickerPhase::CyclePending;
                TimerAction::RunCycle
            }
            _ => TimerAction::Ignore,
        }
    }

    pub fn on_cycle_completed(&mut self) -> Option<Duration> {
        assert!(
            matches!(self.phase, TickerPhase::CyclePending),
            "no cycle is pending"
        );
        self.phase = TickerPhase::Armed;
        self.rearm_if_uncovered()
    }

    /// Restore the invariant after a timer that ran no cycle. Without this a
    /// stray event could consume the last outstanding timer and strand an
    /// armed ticker with nothing left to wake it.
    pub fn ensure_armed(&mut self) -> Option<Duration> {
        match self.phase {
            TickerPhase::Idle | TickerPhase::CyclePending => None,
            TickerPhase::Armed => self.rearm_if_uncovered(),
        }
    }

    fn rearm_if_uncovered(&mut self) -> Option<Duration> {
        if self.outstanding > 0 {
            return None;
        }
        self.outstanding += 1;
        Some(self.interval)
    }

    #[cfg(test)]
    fn is_deadlocked(&self) -> bool {
        !matches!(self.phase, TickerPhase::Idle) && self.outstanding == 0
    }
}

/// Retry gate for publisher initialization. Clock-free for the same reason as
/// [`SampleTicker`]: a timer event is the timer we armed coming due, so there
/// is nothing a deadline comparison can add, and a stale `Instant` would only
/// strand the retry.
#[derive(Debug)]
pub struct RetryTimer {
    interval: Duration,
    armed: bool,
}

impl RetryTimer {
    pub fn new(interval: Duration) -> Self {
        assert!(!interval.is_zero(), "retry interval must be non-zero");
        Self {
            interval,
            armed: false,
        }
    }

    pub fn arm(&mut self) -> Duration {
        self.armed = true;
        self.interval
    }

    /// True once per arming: a duplicate or unsolicited event answers false.
    pub fn on_timer(&mut self) -> bool {
        std::mem::replace(&mut self.armed, false)
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
        let now = self.clock.now();
        self.clamp_to_epoch(now);
        let Some(deadline) = self.timer_deadline else {
            return BroadcasterAction::None;
        };
        if now < deadline {
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

    /// Pull deadlines back into reach of the current clock epoch.
    ///
    /// `change_host_folder` rebuilds the WASI context and restarts
    /// CLOCK_MONOTONIC at zero, so deadlines taken before the swap sit up to a
    /// whole epoch ahead of every later reading. `Instant` still advances, so
    /// this is throttling drift rather than the ticker's deadlock, but left
    /// alone it delays the first publication by that stale offset.
    ///
    /// Nothing here may be scheduled further out than one interval, so a
    /// deadline beyond that horizon cannot be a real one.
    fn clamp_to_epoch(&mut self, now: Instant) {
        let horizon = now + self.interval;
        if self.startup_not_before > horizon {
            self.startup_not_before = horizon;
        }
        for deadline in [
            &mut self.external_not_before,
            &mut self.sink_retry_not_before,
            &mut self.timer_deadline,
        ] {
            if deadline.is_some_and(|value| value > horizon) {
                *deadline = Some(horizon);
            }
        }
        // last_completed is a past instant, not a deadline: a reset epoch makes
        // it look future-dated, which would suppress the next publication.
        if self.last_completed.is_some_and(|value| value > now) {
            self.last_completed = Some(now);
        }
    }

    fn flush_or_schedule(&mut self) -> BroadcasterAction {
        if self.pending.is_none() {
            return BroadcasterAction::None;
        }

        let now = self.clock.now();
        self.clamp_to_epoch(now);
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

/// Seconds between two samples, or `None` when they cannot be compared.
///
/// A rebuilt WASI context restarts CLOCK_MONOTONIC, so `now` can predate the
/// previous sample. Saturating that to zero would hand [`rate`] an empty
/// window, and an empty window answers `0.0` -- a confident "no traffic" for a
/// speed nobody measured. The caller must render `-` instead.
pub fn sample_window(previous: Instant, now: Instant) -> Option<f64> {
    let elapsed = now.checked_duration_since(previous)?;
    (!elapsed.is_zero()).then(|| elapsed.as_secs_f64())
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
mod tests;
