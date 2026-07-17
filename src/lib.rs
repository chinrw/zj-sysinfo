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
