//! Parsers for /proc and macOS command output, plus rate formatting.

use super::super::*;
use super::support::TICK;
use std::time::Instant;

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

    let no_route = format!("{{ 2.85 3.13 3.64 }}\n@@\nroute to: default\n@@\n{MACOS_NETSTAT}\n");
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
/// A restarted CLOCK_MONOTONIC can date the current sample before the
/// previous one. `rate` answers 0.0 for an empty window, so the window has
/// to be rejected here or the widget claims an authoritative "0 B/s".
#[test]
fn sample_window_rejects_incomparable_samples() {
    let now = Instant::now();

    assert_eq!(sample_window(now, now + TICK), Some(TICK.as_secs_f64()));
    assert_eq!(
        sample_window(now + TICK, now),
        None,
        "clock went backwards: the samples are incomparable"
    );
    assert_eq!(
        sample_window(now, now),
        None,
        "an empty window carries no rate information"
    );
}
#[test]
fn format_speed_units() {
    assert_eq!(format_speed(12.0), "12 B/s");
    assert_eq!(format_speed(340.0 * 1024.0), "340 KB/s");
    assert_eq!(format_speed(1.2 * 1024.0 * 1024.0), "1.2 MB/s");
    assert_eq!(format_speed(2.5 * 1024.0 * 1024.0 * 1024.0), "2.5 GB/s");
}
