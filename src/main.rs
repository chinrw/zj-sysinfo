use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use zellij_tile::prelude::*;

use zj_sysinfo::{
    default_iface, fallback_iface, format_speed, iface_bytes, instance_nonce_from_random,
    is_active_client, loadavg, parse_macos_probe, probe_context, probe_token_from_context,
    publication_completion_nonce, rate, sample_window, AsyncProbe, BroadcasterAction, ProbeAction,
    ProbeToken, RetryTimer, SampleTicker, SessionBroadcaster, SharedPublicationLease, SinkAction,
    SystemClock, TimerAction, WidgetSink, WidgetValues, PUBLICATION_COMPLETE_MESSAGE,
};

const INTERVAL: Duration = Duration::from_secs(2);
const MACOS_PROBE: &str = "/usr/sbin/sysctl -n vm.loadavg; echo '@@'; /sbin/route -n get default 2>/dev/null; echo '@@'; /usr/sbin/netstat -ibn 2>/dev/null";
/// Ticks to wait before abandoning an outstanding probe, and the ceiling that
/// wait backs off to. A dropped result must not freeze the widgets forever,
/// but a genuinely hung probe must not get a replacement every few seconds
/// either -- doubling keeps the leak rate decaying instead of linear.
const MISSED_PROBE_TICKS: u32 = 3;
const MISSED_PROBE_TICKS_MAX: u32 = 240;

/// Only Linux is ever latched. Detect re-probes /host/proc every tick, so a
/// read that loses a startup race with change_host_folder can't strand a
/// Linux host on the fork-per-tick command path.
#[derive(Clone, Copy, Default)]
enum HostMode {
    #[default]
    Detect,
    Linux,
}

#[derive(Default)]
enum State {
    #[default]
    Dormant,
    Active(Box<ActivePlugin>),
}

struct ActivePlugin {
    permission_granted: bool,
    publisher: PublisherState,
}

enum PublisherState {
    Initializing {
        plugin_id: u32,
        zellij_pid: u32,
        retry: RetryTimer,
    },
    Running(Box<Runtime>),
}

struct Runtime {
    granted: bool,
    plugin_id: u32,
    instance_nonce: u128,
    ticker: SampleTicker,
    broadcaster: SessionBroadcaster<SystemClock, ZellijSink>,
    host_mode: HostMode,
    probe: AsyncProbe,
    /// (interface name, sample time, rx bytes, tx bytes) of the previous tick.
    prev: Option<(String, Instant, u64, u64)>,
}

impl Runtime {
    fn new(plugin_id: u32, zellij_pid: u32) -> io::Result<Self> {
        let instance_nonce = instance_nonce()?;
        Ok(Self {
            granted: false,
            plugin_id,
            instance_nonce,
            ticker: SampleTicker::new(INTERVAL),
            broadcaster: SessionBroadcaster::new(
                INTERVAL,
                SystemClock,
                ZellijSink {
                    plugin_id,
                    instance_nonce,
                    lease: SharedPublicationLease::new(
                        INTERVAL,
                        PathBuf::from(format!(
                            "/cache/zj-sysinfo-{zellij_pid}-{plugin_id}.publication"
                        )),
                        instance_nonce,
                    ),
                },
            ),
            host_mode: HostMode::default(),
            probe: AsyncProbe::new(instance_nonce, MISSED_PROBE_TICKS, MISSED_PROBE_TICKS_MAX),
            prev: None,
        })
    }
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, _configuration: BTreeMap<String, String>) {
        let ids = get_plugin_ids();
        // Zellij 0.44.3 starts client IDs at 1 and retains this plugin slot
        // after disconnect. Reused ID 1 replaces the slot instead of adding a
        // second active copy, so every other client-scoped copy stays dormant.
        if !is_active_client(ids.client_id) {
            return;
        }
        *self = Self::Active(Box::new(ActivePlugin::new(ids.plugin_id, ids.zellij_pid)));
        request_permission(&[
            PermissionType::FullHdAccess,
            PermissionType::MessageAndLaunchOtherPlugins,
            PermissionType::RunCommands,
        ]);
        subscribe(&[
            EventType::Timer,
            EventType::PermissionRequestResult,
            EventType::RunCommandResult,
        ]);
        if let Self::Active(active) = self {
            active.try_initialize();
        }
    }

    fn update(&mut self, event: Event) -> bool {
        if let Self::Active(active) = self {
            active.update(event);
        }
        false // background plugin: nothing to render
    }

    fn pipe(&mut self, message: PipeMessage) -> bool {
        if let Self::Active(active) = self {
            active.handle_pipe(message);
        }
        false
    }
}

impl ActivePlugin {
    fn new(plugin_id: u32, zellij_pid: u32) -> Self {
        Self {
            permission_granted: false,
            publisher: PublisherState::Initializing {
                plugin_id,
                zellij_pid,
                retry: RetryTimer::new(INTERVAL),
            },
        }
    }

    fn try_initialize(&mut self) {
        let (plugin_id, zellij_pid) = match &self.publisher {
            PublisherState::Initializing {
                plugin_id,
                zellij_pid,
                ..
            } => (*plugin_id, *zellij_pid),
            PublisherState::Running(_) => return,
        };
        match Runtime::new(plugin_id, zellij_pid) {
            Ok(mut runtime) => {
                if self.permission_granted {
                    runtime.grant_permissions();
                }
                self.publisher = PublisherState::Running(Box::new(runtime));
            }
            Err(error) => {
                eprintln!("zj-sysinfo: failed to allocate an instance nonce: {error}");
                let PublisherState::Initializing { retry, .. } = &mut self.publisher else {
                    unreachable!("publisher changed while initializing");
                };
                schedule_timer(retry.arm());
            }
        }
    }

    fn update(&mut self, event: Event) {
        match event {
            Event::PermissionRequestResult(PermissionStatus::Granted) => {
                self.permission_granted = true;
                if let PublisherState::Running(runtime) = &mut self.publisher {
                    runtime.grant_permissions();
                }
            }
            Event::PermissionRequestResult(PermissionStatus::Denied) => {
                eprintln!("zj-sysinfo: permission denied; widgets will stay empty");
                // pipe_message_to_plugin (used by push_widget) needs
                // MessageAndLaunchOtherPlugins, which was just denied, so we
                // can't report through the widgets here. Retry once instead.
                request_permission(&[
                    PermissionType::FullHdAccess,
                    PermissionType::MessageAndLaunchOtherPlugins,
                    PermissionType::RunCommands,
                ]);
            }
            timer_event @ Event::Timer(_) => {
                let should_initialize = match &mut self.publisher {
                    PublisherState::Initializing { retry, .. } => retry.on_timer(),
                    PublisherState::Running(runtime) => {
                        runtime.update(timer_event);
                        false
                    }
                };
                if should_initialize {
                    self.try_initialize();
                }
            }
            event => {
                if let PublisherState::Running(runtime) = &mut self.publisher {
                    runtime.update(event);
                }
            }
        }
    }

    fn handle_pipe(&mut self, message: PipeMessage) {
        if let PublisherState::Running(runtime) = &mut self.publisher {
            runtime.handle_pipe(message);
        }
    }
}

impl Runtime {
    fn grant_permissions(&mut self) {
        if self.granted {
            return;
        }
        self.granted = true;
        change_host_folder(PathBuf::from("/"));
        if let Some(delay) = self.ticker.start() {
            self.schedule(delay);
        }
    }

    /// Single accounted entry point for `set_timeout`. Every timer this plugin
    /// arms has to pass through here, otherwise the ticker's outstanding count
    /// drifts and its no-deadlock invariant stops meaning anything.
    fn schedule(&mut self, delay: Duration) {
        self.ticker.note_schedule();
        schedule_timer(delay);
    }

    fn update(&mut self, event: Event) {
        match event {
            Event::Timer(_) if self.granted => {
                let action = self.ticker.on_timer();
                let broadcast_action = self.broadcaster.on_timer();
                self.handle_broadcaster_action(broadcast_action);
                let rearm = if action == TimerAction::RunCycle {
                    self.tick();
                    self.ticker.on_cycle_completed()
                } else {
                    self.ticker.ensure_armed()
                };
                if let Some(delay) = rearm {
                    self.schedule(delay);
                }
            }
            Event::RunCommandResult(_, stdout, _, context)
                if probe_token_from_context(&context).is_some() =>
            {
                self.handle_macos_probe(&stdout, &context);
            }
            _ => {}
        }
    }

    fn handle_pipe(&mut self, message: PipeMessage) {
        let source_plugin_id = match message.source {
            PipeSource::Plugin(plugin_id) => Some(plugin_id),
            _ => None,
        };
        let Some(nonce) = publication_completion_nonce(
            self.plugin_id,
            source_plugin_id,
            message.is_private,
            &message.name,
            message.payload.as_deref(),
        ) else {
            return;
        };
        if nonce != self.instance_nonce {
            let action = self.broadcaster.observe_external_publication();
            self.handle_broadcaster_action(action);
        }
    }

    fn tick(&mut self) {
        // This host-mounted proc file exists on Linux and not macOS.
        if matches!(self.host_mode, HostMode::Detect)
            && std::fs::read_to_string("/host/proc/net/route").is_ok()
        {
            self.host_mode = HostMode::Linux;
            self.probe.cancel();
        }
        match self.host_mode {
            HostMode::Linux => self.tick_linux(),
            HostMode::Detect => self.tick_macos(),
        }
    }

    fn tick_linux(&mut self) {
        let values = WidgetValues::new(self.netspeed_text(), loadavg_text());
        self.submit(values);
    }

    fn netspeed_text(&mut self) -> String {
        self.netspeed_text_from(read_counters())
    }

    fn netspeed_text_from(&mut self, counters: Option<(String, (u64, u64))>) -> String {
        let Some((iface, (rx, tx))) = counters else {
            self.prev = None;
            return "-".to_string();
        };
        let now = Instant::now();
        // A different default interface than last tick (e.g. link
        // failover) makes the previous sample incomparable -- treat it as
        // if there were no previous sample at all.
        let text = match &self.prev {
            Some((prev_iface, at, prev_rx, prev_tx)) if *prev_iface == iface => {
                match sample_window(*at, now) {
                    Some(elapsed) => format!(
                        "D: {} U: {}",
                        format_speed(rate(*prev_rx, rx, elapsed)),
                        format_speed(rate(*prev_tx, tx, elapsed)),
                    ),
                    None => "-".to_string(),
                }
            }
            _ => "-".to_string(),
        };
        self.prev = Some((iface, now, rx, tx));
        text
    }

    fn tick_macos(&mut self) {
        match self.probe.on_cycle() {
            ProbeAction::Start(token) => self.launch_macos_probe(token),
            ProbeAction::Wait => {}
            ProbeAction::Restart(token) => {
                self.prev = None;
                self.submit(WidgetValues::new("-", "-"));
                self.launch_macos_probe(token);
            }
        }
    }

    fn launch_macos_probe(&self, token: ProbeToken) {
        run_command(&["/bin/sh", "-c", MACOS_PROBE], probe_context(token));
    }

    fn handle_macos_probe(&mut self, stdout: &[u8], context: &BTreeMap<String, String>) {
        let Some(token) = probe_token_from_context(context) else {
            return;
        };
        if !self.probe.complete(token) {
            return;
        }
        let sample = std::str::from_utf8(stdout).ok().and_then(parse_macos_probe);
        let (counters, loadavg) = match sample {
            Some(sample) => (sample.counters, sample.loadavg),
            None => (None, None),
        };

        let netspeed = self.netspeed_text_from(counters);
        let loadavg = loadavg
            .map(|(one, five)| format!("{one} {five}"))
            .unwrap_or_else(|| "-".to_string());
        self.submit(WidgetValues::new(netspeed, loadavg));
    }

    fn submit(&mut self, values: WidgetValues) {
        let action = self.broadcaster.submit(values);
        self.handle_broadcaster_action(action);
    }

    fn handle_broadcaster_action(&mut self, action: BroadcasterAction) {
        if let BroadcasterAction::Schedule(delay) = action {
            self.schedule(delay);
        }
    }
}

struct ZellijSink {
    plugin_id: u32,
    instance_nonce: u128,
    lease: SharedPublicationLease,
}

impl WidgetSink for ZellijSink {
    fn publish(&mut self, values: &WidgetValues) -> SinkAction {
        self.lease.publish(|| {
            push_widget("pipe_netspeed", &values.netspeed);
            push_widget("pipe_uptime", &values.uptime);
            pipe_message_to_plugin(
                MessageToPlugin::new(PUBLICATION_COMPLETE_MESSAGE)
                    .with_destination_plugin_id(self.plugin_id)
                    .with_payload(self.instance_nonce.to_string()),
            );
        })
    }
}

fn instance_nonce() -> io::Result<u128> {
    let mut random = [0; 16];
    getrandom::getrandom(&mut random)
        .map_err(|error| io::Error::other(format!("random source failed ({error})")))?;
    Ok(instance_nonce_from_random(random))
}

fn schedule_timer(delay: Duration) {
    set_timeout(delay.as_secs_f64());
}

/// (interface name, (rx bytes, tx bytes)) of the default-route interface,
/// read through /host.
fn read_counters() -> Option<(String, (u64, u64))> {
    let route = std::fs::read_to_string("/host/proc/net/route").ok()?;
    let dev = std::fs::read_to_string("/host/proc/net/dev").ok()?;
    let iface = default_iface(&route).or_else(|| fallback_iface(&dev))?;
    let bytes = iface_bytes(&dev, &iface)?;
    Some((iface, bytes))
}

fn loadavg_text() -> String {
    std::fs::read_to_string("/host/proc/loadavg")
        .ok()
        .and_then(|s| loadavg(&s))
        .map(|(one, five)| format!("{one} {five}"))
        .unwrap_or_else(|| "-".to_string())
}

/// Broadcast one zjstatus pipe-widget update to all plugins in the session.
fn push_widget(widget: &str, text: &str) {
    // zjstatus's pipe() parses the *payload* with its line protocol; the
    // message name is irrelevant. Newlines would break the protocol.
    let payload = format!("zjstatus::pipe::{widget}::{}", text.replace('\n', " "));
    pipe_message_to_plugin(MessageToPlugin::new("zjstatus").with_payload(payload));
}
