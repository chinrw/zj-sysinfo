use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use zellij_tile::prelude::*;

use zj_sysinfo::{default_iface, fallback_iface, format_speed, iface_bytes, loadavg, rate};

const INTERVAL_SECS: f64 = 2.0;

#[derive(Default)]
struct State {
    granted: bool,
    /// (interface name, sample time, rx bytes, tx bytes) of the previous tick.
    prev: Option<(String, Instant, u64, u64)>,
}

register_plugin!(State);

impl ZellijPlugin for State {
    fn load(&mut self, _configuration: BTreeMap<String, String>) {
        request_permission(&[
            PermissionType::FullHdAccess,
            PermissionType::MessageAndLaunchOtherPlugins,
        ]);
        subscribe(&[EventType::Timer, EventType::PermissionRequestResult]);
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
                ]);
            },
            Event::Timer(_) => {
                self.tick();
                set_timeout(INTERVAL_SECS);
            },
            _ => {},
        }
        false // background plugin: nothing to render
    }
}

impl State {
    fn tick(&mut self) {
        push_widget("pipe_netspeed", &self.netspeed_text());
        push_widget("pipe_uptime", &loadavg_text());
    }

    fn netspeed_text(&mut self) -> String {
        let Some((iface, (rx, tx))) = read_counters() else {
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
