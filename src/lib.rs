use std::time::{Duration, Instant};

// ListClients is derived from pane metadata and can omit a live client during
// startup. Missing copies retry forever, but back off to limit zombie work.
const MAX_MISSING_CLIENT_RETRY: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimerAction {
    Ignore,
    QueryClients,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientAction {
    Ignore,
    Schedule(Duration),
    Sample,
}

#[derive(Debug)]
enum TickerPhase {
    Idle,
    TimerArmed(Instant),
    ClientQueryInFlight,
    SamplePending,
}

#[derive(Debug)]
pub struct SessionTicker {
    interval: Duration,
    phase: TickerPhase,
    missing_client_polls: u8,
    last_publication: Option<Instant>,
}

impl SessionTicker {
    pub fn new(interval: Duration) -> Self {
        assert!(!interval.is_zero(), "ticker interval must be non-zero");
        Self {
            interval,
            phase: TickerPhase::Idle,
            missing_client_polls: 0,
            last_publication: None,
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
                self.phase = TickerPhase::ClientQueryInFlight;
                TimerAction::QueryClients
            }
            _ => TimerAction::Ignore,
        }
    }

    pub fn on_clients<I>(
        &mut self,
        now: Instant,
        own_client_id: u16,
        connected_client_ids: I,
    ) -> ClientAction
    where
        I: IntoIterator<Item = u16>,
    {
        if !matches!(self.phase, TickerPhase::ClientQueryInFlight) {
            return ClientAction::Ignore;
        }
        let mut own_client_is_connected = false;
        let mut publisher = None;
        for client_id in connected_client_ids {
            own_client_is_connected |= client_id == own_client_id;
            publisher = Some(publisher.map_or(client_id, |current: u16| current.min(client_id)));
        }
        if !own_client_is_connected {
            self.missing_client_polls = self.missing_client_polls.saturating_add(1);
            self.last_publication = None;
            let delay = self.missing_client_retry();
            self.arm(now, delay);
            return ClientAction::Schedule(delay);
        }

        self.missing_client_polls = 0;
        if publisher == Some(own_client_id) {
            self.phase = TickerPhase::SamplePending;
            ClientAction::Sample
        } else {
            self.last_publication = None;
            let delay = self.interval;
            self.arm(now, delay);
            ClientAction::Schedule(delay)
        }
    }

    pub fn on_sample_cycle_completed(&mut self, now: Instant) -> Duration {
        assert!(
            matches!(self.phase, TickerPhase::SamplePending),
            "no sample cycle is pending"
        );
        let delay = self.interval;
        self.arm(now, delay);
        delay
    }

    pub fn allow_publication(&mut self, now: Instant) -> bool {
        if self
            .last_publication
            .is_some_and(|last| now.saturating_duration_since(last) < self.interval)
        {
            return false;
        }
        self.last_publication = Some(now);
        true
    }

    fn arm(&mut self, now: Instant, delay: Duration) {
        self.phase = TickerPhase::TimerArmed(now + delay);
    }

    fn missing_client_retry(&self) -> Duration {
        let exponent = u32::from(self.missing_client_polls.saturating_sub(1)).min(31);
        let multiplier = 1_u32.checked_shl(exponent).unwrap_or(u32::MAX);
        self.interval
            .saturating_mul(multiplier)
            .min(MAX_MISSING_CLIENT_RETRY.max(self.interval))
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
    use std::time::Duration;

    const TICK: Duration = Duration::from_secs(2);

    fn ticker() -> SessionTicker {
        SessionTicker::new(TICK)
    }

    #[test]
    #[should_panic(expected = "ticker interval must be non-zero")]
    fn ticker_rejects_zero_interval() {
        SessionTicker::new(Duration::ZERO);
    }

    #[test]
    fn client_scoped_instances_choose_one_session_publisher() {
        let now = Instant::now();
        let mut first = ticker();
        let mut second = ticker();

        assert_eq!(first.start(now), Some(Duration::ZERO));
        assert_eq!(second.start(now), Some(Duration::ZERO));
        assert_eq!(first.on_timer(now), TimerAction::QueryClients);
        assert_eq!(second.on_timer(now), TimerAction::QueryClients);

        let actions = [
            first.on_clients(now, 1, [1, 2]),
            second.on_clients(now, 2, [1, 2]),
        ];
        assert_eq!(
            actions
                .iter()
                .filter(|action| matches!(action, ClientAction::Sample))
                .count(),
            1
        );
        assert_eq!(first.on_sample_cycle_completed(now), TICK);
    }

    #[test]
    fn duplicate_timer_events_do_not_fork_the_tick_loop() {
        let now = Instant::now();
        let mut ticker = ticker();

        assert_eq!(ticker.start(now), Some(Duration::ZERO));
        assert_eq!(ticker.on_timer(now), TimerAction::QueryClients);
        assert_eq!(ticker.on_timer(now), TimerAction::Ignore);
        assert_eq!(ticker.on_clients(now, 1, [1]), ClientAction::Sample);
        assert_eq!(ticker.on_sample_cycle_completed(now), TICK);
        assert_eq!(
            ticker.on_timer(now + Duration::from_secs(1)),
            TimerAction::Ignore
        );
        assert_eq!(ticker.on_timer(now + TICK), TimerAction::QueryClients);
    }

    #[test]
    fn session_ticker_review_regression_anchors_publish_interval() {
        let now = Instant::now();
        let response_latency = Duration::from_millis(1_800);
        let first_snapshot = now + response_latency;
        let publication_work = Duration::from_millis(400);
        let mut ticker = ticker();

        ticker.start(now);
        ticker.on_timer(now);

        assert_eq!(
            ticker.on_clients(first_snapshot, 1, [1]),
            ClientAction::Sample
        );
        assert_eq!(ticker.on_timer(now + TICK), TimerAction::Ignore);

        let first_publication = first_snapshot + publication_work;
        assert_eq!(ticker.on_sample_cycle_completed(first_publication), TICK);
        assert_eq!(ticker.on_timer(first_snapshot + TICK), TimerAction::Ignore);

        let next_query = first_publication + TICK;
        assert_eq!(ticker.on_timer(next_query), TimerAction::QueryClients);
        let second_snapshot = next_query + Duration::from_millis(100);
        assert_eq!(
            ticker.on_clients(second_snapshot, 1, [1]),
            ClientAction::Sample
        );
        let second_publication = second_snapshot + Duration::from_millis(100);
        assert_eq!(ticker.on_sample_cycle_completed(second_publication), TICK);
        assert!(second_publication.duration_since(first_publication) >= TICK);
    }

    #[test]
    fn async_results_cannot_compress_publication_interval() {
        let now = Instant::now();
        let mut ticker = ticker();
        let first_result = now + Duration::from_millis(1_900);

        assert!(ticker.allow_publication(first_result));
        assert!(!ticker.allow_publication(now + Duration::from_millis(2_100)));
        assert!(ticker.allow_publication(first_result + TICK));
    }

    #[test]
    fn session_ticker_review_regression_recovers_after_omissions() {
        let mut now = Instant::now();
        let mut ticker = ticker();

        ticker.start(now);
        ticker.on_timer(now);
        for delay in [2, 4, 8, 16, 30, 30].map(Duration::from_secs) {
            assert_eq!(ticker.on_clients(now, 1, []), ClientAction::Schedule(delay));
            now += delay;
            assert_eq!(ticker.on_timer(now), TimerAction::QueryClients);
        }

        assert_eq!(ticker.on_clients(now, 1, [1]), ClientAction::Sample);
        assert_eq!(ticker.on_sample_cycle_completed(now), TICK);

        now += TICK;
        assert_eq!(ticker.on_timer(now), TimerAction::QueryClients);
        assert_eq!(ticker.on_clients(now, 1, []), ClientAction::Schedule(TICK));
    }

    #[test]
    fn connected_follower_takes_over_when_the_publisher_disconnects() {
        let now = Instant::now();
        let mut first = ticker();
        let mut second = ticker();

        first.start(now);
        second.start(now);
        first.on_timer(now);
        second.on_timer(now);
        assert!(matches!(
            first.on_clients(now, 1, [1, 2]),
            ClientAction::Sample
        ));
        first.on_sample_cycle_completed(now);
        assert!(matches!(
            second.on_clients(now, 2, [1, 2]),
            ClientAction::Schedule(_)
        ));

        first.on_timer(now + TICK);
        second.on_timer(now + TICK);
        let actions = [
            first.on_clients(now + TICK, 1, [2]),
            second.on_clients(now + TICK, 2, [2]),
        ];
        assert_eq!(
            actions
                .iter()
                .filter(|action| matches!(action, ClientAction::Sample))
                .count(),
            1
        );
        second.on_sample_cycle_completed(now + TICK);
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
