use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use zellij_tile::prelude::*;

use zj_sysinfo::{
    default_iface, fallback_iface, format_speed, iface_bytes, loadavg, parse_macos_probe, rate,
};

const INTERVAL_SECS: f64 = 2.0;
const MACOS_PROBE: &str = "/usr/sbin/sysctl -n vm.loadavg; echo '@@'; /sbin/route -n get default 2>/dev/null; echo '@@'; /usr/sbin/netstat -ibn 2>/dev/null";
const PROBE_CONTEXT_KEY: &str = "zj-sysinfo";
const PROBE_CONTEXT_VALUE: &str = "macos-sysinfo";
const PROBE_CONTEXT_GENERATION_KEY: &str = "generation";
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

struct State {
    granted: bool,
    host_mode: HostMode,
    probe_in_flight: bool,
    missed_ticks: u32,
    abandon_after: u32,
    /// Stamped into each probe's context so a result we already gave up on
    /// can be told apart from the one currently in flight.
    generation: u64,
    /// (interface name, sample time, rx bytes, tx bytes) of the previous tick.
    prev: Option<(String, Instant, u64, u64)>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            granted: false,
            host_mode: HostMode::default(),
            probe_in_flight: false,
            missed_ticks: 0,
            abandon_after: MISSED_PROBE_TICKS,
            generation: 0,
            prev: None,
        }
    }
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, _configuration: BTreeMap<String, String>) {
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
    }

    fn update(&mut self, event: Event) -> bool {
        match event {
            Event::PermissionRequestResult(PermissionStatus::Granted) if !self.granted => {
                self.granted = true;
                change_host_folder(PathBuf::from("/"));
                set_timeout(0.0); // first tick immediately
            },
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
            },
            Event::Timer(_) => {
                self.tick();
                set_timeout(INTERVAL_SECS);
            },
            Event::RunCommandResult(_, stdout, _, context) if is_macos_probe_result(&context) => {
                self.handle_macos_probe(&stdout, &context);
            },
            _ => {},
        }
        false // background plugin: nothing to render
    }
}

impl State {
    fn tick(&mut self) {
        // This host-mounted proc file exists on Linux and not macOS.
        if matches!(self.host_mode, HostMode::Detect)
            && std::fs::read_to_string("/host/proc/net/route").is_ok()
        {
            self.host_mode = HostMode::Linux;
        }
        match self.host_mode {
            HostMode::Linux => self.tick_linux(),
            HostMode::Detect => self.start_macos_probe(),
        }
    }

    fn tick_linux(&mut self) {
        push_widget("pipe_netspeed", &self.netspeed_text());
        push_widget("pipe_uptime", &loadavg_text());
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
                let elapsed = now.duration_since(*at).as_secs_f64();
                format!(
                    "D: {} U: {}",
                    format_speed(rate(*prev_rx, rx, elapsed)),
                    format_speed(rate(*prev_tx, tx, elapsed)),
                )
            },
            _ => "-".to_string(),
        };
        self.prev = Some((iface, now, rx, tx));
        text
    }

    fn start_macos_probe(&mut self) {
        if self.probe_in_flight {
            self.missed_ticks += 1;
            if self.missed_ticks < self.abandon_after {
                return;
            }
            self.abandon_after = (self.abandon_after * 2).min(MISSED_PROBE_TICKS_MAX);
            // Stop presenting the abandoned probe's readings as current.
            self.report_unavailable();
        }
        self.missed_ticks = 0;
        self.probe_in_flight = true;
        self.generation = self.generation.wrapping_add(1);
        let mut context = BTreeMap::new();
        context.insert(
            PROBE_CONTEXT_KEY.to_string(),
            PROBE_CONTEXT_VALUE.to_string(),
        );
        context.insert(
            PROBE_CONTEXT_GENERATION_KEY.to_string(),
            self.generation.to_string(),
        );
        run_command(&["/bin/sh", "-c", MACOS_PROBE], context);
    }

    fn report_unavailable(&mut self) {
        self.prev = None;
        push_widget("pipe_netspeed", "-");
        push_widget("pipe_uptime", "-");
    }

    fn handle_macos_probe(&mut self, stdout: &[u8], context: &BTreeMap<String, String>) {
        // A result for a probe we already abandoned would clear the flag for
        // the replacement still in flight, defeating the backoff, and would
        // feed an out-of-order sample into `prev`.
        let generation = context
            .get(PROBE_CONTEXT_GENERATION_KEY)
            .and_then(|g| g.parse::<u64>().ok());
        if generation != Some(self.generation) {
            return;
        }

        self.probe_in_flight = false;
        self.missed_ticks = 0;
        self.abandon_after = MISSED_PROBE_TICKS;
        let sample = std::str::from_utf8(stdout).ok().and_then(parse_macos_probe);
        let (counters, loadavg) = match sample {
            Some(sample) => (sample.counters, sample.loadavg),
            None => (None, None),
        };

        let netspeed = self.netspeed_text_from(counters);
        let loadavg = loadavg
            .map(|(one, five)| format!("{one} {five}"))
            .unwrap_or_else(|| "-".to_string());
        push_widget("pipe_netspeed", &netspeed);
        push_widget("pipe_uptime", &loadavg);
    }
}

fn is_macos_probe_result(context: &BTreeMap<String, String>) -> bool {
    context.get(PROBE_CONTEXT_KEY).map(String::as_str) == Some(PROBE_CONTEXT_VALUE)
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
