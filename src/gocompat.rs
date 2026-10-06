//! Go standard-library behavior the port must reproduce exactly: duration
//! syntax (stored in the shared config file), RFC 3339 log timestamps, process
//! status text, byte decoding, and sort.SliceStable's ordering.

use std::os::unix::process::ExitStatusExt;
use std::process::ExitStatus;
use std::time::Duration;

use chrono::{DateTime, Datelike, FixedOffset, Timelike};

const MAX_DURATION: u64 = 1 << 63;

/// Parses Go's time.ParseDuration syntax ("45m", "1h30m", "1.5h"). Negative
/// durations other than zero are rejected because no caller accepts them.
pub fn parse_duration(input: &str) -> Option<Duration> {
    let (negative, mut rest) = match input.as_bytes().first() {
        Some(b'-') => (true, &input[1..]),
        Some(b'+') => (false, &input[1..]),
        _ => (false, input),
    };
    if rest == "0" {
        return Some(Duration::ZERO);
    }
    if rest.is_empty() {
        return None;
    }
    let mut total: u64 = 0;
    while !rest.is_empty() {
        let first = rest.as_bytes()[0];
        if first != b'.' && !first.is_ascii_digit() {
            return None;
        }
        let int_len = rest.bytes().take_while(u8::is_ascii_digit).count();
        let mut value: u64 = 0;
        for digit in rest[..int_len].bytes() {
            if value > MAX_DURATION / 10 {
                return None;
            }
            value = value * 10 + u64::from(digit - b'0');
            if value > MAX_DURATION {
                return None;
            }
        }
        rest = &rest[int_len..];
        let (mut fraction, mut scale, mut has_fraction) = (0u64, 1f64, false);
        if let Some(after_dot) = rest.strip_prefix('.') {
            let frac_len = after_dot.bytes().take_while(u8::is_ascii_digit).count();
            let mut overflow = false;
            for digit in after_dot[..frac_len].bytes() {
                if overflow {
                    continue;
                }
                if fraction > (MAX_DURATION - 1) / 10 {
                    overflow = true;
                    continue;
                }
                let next = fraction * 10 + u64::from(digit - b'0');
                if next > MAX_DURATION {
                    overflow = true;
                    continue;
                }
                fraction = next;
                scale *= 10.0;
            }
            has_fraction = frac_len > 0;
            rest = &after_dot[frac_len..];
        }
        if int_len == 0 && !has_fraction {
            return None;
        }
        let unit_len = rest.bytes().take_while(|b| *b != b'.' && !b.is_ascii_digit()).count();
        let unit: u64 = match &rest[..unit_len] {
            "ns" => 1,
            "us" | "\u{b5}s" | "\u{3bc}s" => 1_000,
            "ms" => 1_000_000,
            "s" => 1_000_000_000,
            "m" => 60_000_000_000,
            "h" => 3_600_000_000_000,
            _ => return None,
        };
        rest = &rest[unit_len..];
        if value > MAX_DURATION / unit {
            return None;
        }
        value *= unit;
        if fraction > 0 {
            value += (fraction as f64 * (unit as f64 / scale)) as u64;
            if value > MAX_DURATION {
                return None;
            }
        }
        total += value;
        if total > MAX_DURATION {
            return None;
        }
    }
    if negative {
        return (total == 0).then_some(Duration::ZERO);
    }
    if total > MAX_DURATION - 1 {
        return None;
    }
    Some(Duration::from_nanos(total))
}

/// Formats like Go's Duration.String ("30m0s", "2h0m0s", "50ms").
pub fn format_duration(d: Duration) -> String {
    let nanos = u64::try_from(d.as_nanos()).unwrap_or(u64::MAX);
    if nanos < 1_000_000_000 {
        if nanos == 0 {
            return "0s".to_string();
        }
        let (precision, unit) = match nanos {
            0..1_000 => (0, "ns"),
            1_000..1_000_000 => (3, "\u{b5}s"),
            _ => (6, "ms"),
        };
        let (fraction, whole) = format_fraction(nanos, precision);
        return format!("{whole}{fraction}{unit}");
    }
    let (fraction, seconds) = format_fraction(nanos, 9);
    let mut out = format!("{}{fraction}s", seconds % 60);
    let minutes = seconds / 60;
    if minutes > 0 {
        out = format!("{}m{out}", minutes % 60);
        let hours = minutes / 60;
        if hours > 0 {
            out = format!("{hours}h{out}");
        }
    }
    out
}

/// Splits off `precision` fractional digits, dropping trailing zeros and the
/// point itself when nothing remains.
fn format_fraction(mut value: u64, precision: u32) -> (String, u64) {
    let mut digits = Vec::new();
    let mut print = false;
    for _ in 0..precision {
        let digit = value % 10;
        print = print || digit != 0;
        if print {
            digits.push(char::from(b'0' + digit as u8));
        }
        value /= 10;
    }
    if print {
        digits.push('.');
    }
    (digits.iter().rev().collect(), value)
}

pub fn parse_rfc3339(value: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(value).ok()
}

/// Formats with Go's RFC3339Nano layout: trailing fractional zeros trimmed
/// and "Z" for a zero offset.
pub fn format_rfc3339_nano(t: &DateTime<FixedOffset>) -> String {
    let mut out =
        format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}", t.year(), t.month(), t.day(), t.hour(), t.minute(), t.second());
    let nanos = t.nanosecond() % 1_000_000_000;
    if nanos > 0 {
        out.push('.');
        out.push_str(format!("{nanos:09}").trim_end_matches('0'));
    }
    let offset = t.offset().local_minus_utc();
    if offset == 0 {
        out.push('Z');
    } else {
        let sign = if offset < 0 { '-' } else { '+' };
        let minutes = offset.abs() / 60;
        out.push_str(&format!("{sign}{:02}:{:02}", minutes / 60, minutes % 60));
    }
    out
}

/// Renders a non-success status the way Go's ProcessState does.
pub fn exit_status_text(status: ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exit status {code}");
    }
    match status.signal() {
        Some(signal) => format!("signal: {}", signal_name(signal)),
        None => status.to_string(),
    }
}

fn signal_name(signal: i32) -> String {
    let name = match signal {
        libc::SIGHUP => "hangup",
        libc::SIGINT => "interrupt",
        libc::SIGQUIT => "quit",
        libc::SIGKILL => "killed",
        libc::SIGSEGV => "segmentation fault",
        libc::SIGPIPE => "broken pipe",
        libc::SIGTERM => "terminated",
        libc::SIGABRT if cfg!(target_os = "macos") => "abort trap",
        libc::SIGABRT => "aborted",
        _ => return format!("signal {signal}"),
    };
    name.to_string()
}

/// Decodes command output that Go would keep as raw bytes. Invalid C1 bytes
/// map to their control code points so sanitizing still strips the escape
/// sequences they introduce; other invalid bytes become U+FFFD.
pub fn decode_bytes(bytes: &[u8]) -> String {
    if let Ok(valid) = std::str::from_utf8(bytes) {
        return valid.to_owned();
    }
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        out.push_str(chunk.valid());
        for &byte in chunk.invalid() {
            out.push(if (0x80..=0x9f).contains(&byte) { char::from(byte) } else { '\u{fffd}' });
        }
    }
    out
}

/// Go's sort.SliceStable algorithm. Container ordering uses a comparator
/// that is not a strict weak order, so std's sort may order differently or
/// panic; this reproduces Go's exact output.
pub fn stable_sort_by<T>(data: &mut [T], less: impl Fn(&T, &T) -> bool) {
    let n = data.len();
    let mut block = 20;
    let (mut a, mut b) = (0, block);
    while b <= n {
        insertion_sort(data, a, b, &less);
        a = b;
        b += block;
    }
    insertion_sort(data, a, n, &less);
    while block < n {
        let (mut a, mut b) = (0, 2 * block);
        while b <= n {
            sym_merge(data, a, a + block, b, &less);
            a = b;
            b += 2 * block;
        }
        let m = a + block;
        if m < n {
            sym_merge(data, a, m, n, &less);
        }
        block *= 2;
    }
}

fn insertion_sort<T>(data: &mut [T], a: usize, b: usize, less: &impl Fn(&T, &T) -> bool) {
    for i in a + 1..b {
        let mut j = i;
        while j > a && less(&data[j], &data[j - 1]) {
            data.swap(j, j - 1);
            j -= 1;
        }
    }
}

fn sym_merge<T>(data: &mut [T], a: usize, m: usize, b: usize, less: &impl Fn(&T, &T) -> bool) {
    if m - a == 1 {
        let (mut i, mut j) = (m, b);
        while i < j {
            let h = (i + j) / 2;
            if less(&data[h], &data[a]) {
                i = h + 1;
            } else {
                j = h;
            }
        }
        for k in a..i.saturating_sub(1) {
            data.swap(k, k + 1);
        }
        return;
    }
    if b - m == 1 {
        let (mut i, mut j) = (a, m);
        while i < j {
            let h = (i + j) / 2;
            if !less(&data[m], &data[h]) {
                i = h + 1;
            } else {
                j = h;
            }
        }
        let mut k = m;
        while k > i {
            data.swap(k, k - 1);
            k -= 1;
        }
        return;
    }
    let mid = (a + b) / 2;
    let n = mid + m;
    let (mut start, mut r) = if m > mid { (n - b, mid) } else { (a, m) };
    let p = n - 1;
    while start < r {
        let c = (start + r) / 2;
        if !less(&data[p - c], &data[c]) {
            start = c + 1;
        } else {
            r = c;
        }
    }
    let end = n - start;
    if start < m && m < end {
        rotate(data, start, m, end);
    }
    if a < start && start < mid {
        sym_merge(data, a, start, mid, less);
    }
    if mid < end && end < b {
        sym_merge(data, mid, end, b, less);
    }
}

fn rotate<T>(data: &mut [T], a: usize, m: usize, b: usize) {
    let (mut i, mut j) = (m - a, b - m);
    while i != j {
        if i > j {
            swap_range(data, m - i, m, j);
            i -= j;
        } else {
            swap_range(data, m - i, m + j - i, i);
            j -= i;
        }
    }
    swap_range(data, m - i, m, i);
}

fn swap_range<T>(data: &mut [T], a: usize, b: usize, n: usize) {
    for i in 0..n {
        data.swap(a + i, b + i);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_go_durations() {
        let min = Duration::from_secs(60);
        for (input, want) in [
            ("45m", Some(45 * min)),
            ("2h", Some(120 * min)),
            ("1h30m", Some(90 * min)),
            ("1.5h", Some(90 * min)),
            ("30m0s", Some(30 * min)),
            ("90s", Some(Duration::from_secs(90))),
            ("300ms", Some(Duration::from_millis(300))),
            ("0", Some(Duration::ZERO)),
            ("-0s", Some(Duration::ZERO)),
            ("-5m", None),
            ("", None),
            ("soon", None),
            ("30", None),
            ("30M", None),
            (".m", None),
        ] {
            assert_eq!(parse_duration(input), want, "parse_duration({input:?})");
        }
    }

    #[test]
    fn formats_go_durations() {
        for (input, want) in [
            (Duration::ZERO, "0s"),
            (Duration::from_millis(50), "50ms"),
            (Duration::from_micros(1500), "1.5ms"),
            (Duration::from_secs(15), "15s"),
            (Duration::from_secs(30 * 60), "30m0s"),
            (Duration::from_secs(2 * 3600), "2h0m0s"),
            (Duration::from_millis(90_500), "1m30.5s"),
        ] {
            assert_eq!(format_duration(input), want);
        }
    }

    #[test]
    fn rfc3339_nano_round_trip() {
        let t = parse_rfc3339("2024-01-02T03:04:05.000000001Z").unwrap();
        let next = t + chrono::Duration::nanoseconds(1);
        assert_eq!(format_rfc3339_nano(&next), "2024-01-02T03:04:05.000000002Z");
        let t = parse_rfc3339("2024-01-02T03:04:05.120+02:00").unwrap();
        assert_eq!(format_rfc3339_nano(&t), "2024-01-02T03:04:05.12+02:00");
        assert!(parse_rfc3339("plain").is_none());
    }

    #[test]
    fn stable_sort_matches_go_for_non_transitive_comparator() {
        // Go's SliceStable with the container comparator leaves equal-rank
        // states in input order; a by-name sort would swap these.
        let mut states = vec![("exited", "b"), ("created", "a"), ("running", "z"), ("running", "c")];
        stable_sort_by(&mut states, |a, b| if a.0 == b.0 { a.1 < b.1 } else { a.0 == "running" });
        assert_eq!(states, [("running", "c"), ("running", "z"), ("exited", "b"), ("created", "a")]);
    }

    #[test]
    fn stable_sort_handles_merge_blocks() {
        let mut values: Vec<(u32, usize)> = (0..97).map(|i| ((i * 7919 % 13) as u32, i)).collect();
        stable_sort_by(&mut values, |a, b| a.0 < b.0);
        assert!(values.windows(2).all(|w| w[0].0 < w[1].0 || (w[0].0 == w[1].0 && w[0].1 < w[1].1)));
    }

    #[test]
    fn decodes_raw_c1_bytes_as_controls() {
        assert_eq!(decode_bytes(b"a\x9bb\xffc"), "a\u{9b}b\u{fffd}c");
        assert_eq!(decode_bytes("数据".as_bytes()), "数据");
    }

    #[test]
    fn exit_text_matches_go() {
        assert_eq!(exit_status_text(ExitStatus::from_raw(3 << 8)), "exit status 3");
        assert_eq!(exit_status_text(ExitStatus::from_raw(libc::SIGKILL)), "signal: killed");
    }
}
