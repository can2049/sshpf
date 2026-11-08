//! Record formatting: the timestamp and the two token helpers that make a record
//! both machine-parseable and readable.
//!
//! # Layout
//!
//! ```text
//! 2026-11-08T18:20:01.123+08:00 INFO  seq=42 event=tunnel.ready tunnel=3 name=cloud elapsed_ms=812
//! ```
//!
//! * the timestamp column comes from [`now_timestamp`];
//! * keys and `event=` values go through [`token`], so they can never contain the
//!   separators the reader splits on;
//! * every other value goes through [`push_value`], which quotes and escapes only
//!   when a bare token would be ambiguous.
//!
//! Everything in this file is pure: no file, no global state, no lock. That is
//! deliberate — `now_timestamp` is called from inside the panic hook, and the
//! token helpers are called while the sink lock is held, so they must be cheap
//! and must not panic.

use std::fmt::Write as _;

/// `2026-11-08T18:20:01.123+08:00` — millisecond precision in local time, which
/// is what lets sshpf records line up with `journalctl` and sshd output.
///
/// Deliberately **not** `chrono::Local::now()`: that is `Utc::now()` followed by a
/// local-offset lookup, and `Utc::now()` is
/// `SystemTime::now().duration_since(UNIX_EPOCH).expect("system time before Unix
/// epoch")`. A board without an RTC — or a container started before its clock is
/// synced — has a pre-epoch clock, and this function runs inside the panic hook,
/// where a nested panic aborts the process instead of leaving a crash record.
/// Building the instant by hand cannot panic; a pre-epoch clock simply formats as
/// 1970-01-01.
pub fn now_timestamp() -> String {
    const FORMAT: &str = "%Y-%m-%dT%H:%M:%S%.3f%:z";
    let elapsed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let secs = i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX);
    let Some(utc) = chrono::DateTime::from_timestamp(secs, elapsed.subsec_nanos()) else {
        // Only reachable with a clock set past the representable range; keep the
        // record instead of losing it.
        return format!("epoch={secs}");
    };
    // The local-offset lookup is the one third-party call here that can still
    // panic: `chrono`'s `Local::offset_from_utc_datetime` is
    // `.expect("unable to select local time type")` over a fallible TZif lookup,
    // which a corrupt `/usr/share/zoneinfo` can fail. There is no fallible public
    // API to use instead, and this function is on the panic-hook path, where a
    // nested panic aborts the process — so `catch_unwind` here is the documented
    // last-resort case, and the fallback (UTC) is a fully valid record rather than
    // a lost one.
    std::panic::catch_unwind(|| utc.with_timezone(&chrono::Local).format(FORMAT).to_string())
        .unwrap_or_else(|_| utc.format(FORMAT).to_string())
}

/// Keys and event names are parsed by splitting on `=` and whitespace, so either
/// would corrupt the record.
pub(super) fn token(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        out.push(if c.is_whitespace() || c == '=' || c == '"' || c == '\\' || c.is_control() { '_' } else { c });
    }
    if out.is_empty() { "_".to_string() } else { out }
}

/// Append a value, quoting and escaping it when a bare token would be ambiguous.
pub(super) fn push_value(out: &mut String, value: &str) {
    let needs_quoting =
        value.is_empty() || value.chars().any(|c| c.is_whitespace() || c == '"' || c == '\\' || c.is_control());
    if !needs_quoting {
        out.push_str(value);
        return;
    }
    out.push('"');
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                // `write!` into a `String` is infallible; the result is dropped
                // because there is nothing to report and nowhere to report it.
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render one value exactly as a record would, for the escaping tests below.
    fn value(text: &str) -> String {
        let mut out = String::new();
        push_value(&mut out, text);
        out
    }

    /// `now_timestamp` cannot panic: it is called from inside the panic hook.
    #[test]
    fn the_record_timestamp_is_always_well_formed() {
        // The documented shape, whatever the clock says.
        let ts = now_timestamp();
        assert!(ts.len() >= 19, "no timestamp at all: {ts:?}");
        // Either the RFC 3339 form or the `epoch=` fallback for a clock past 9999.
        assert!(
            ts.starts_with("epoch=") || (ts.chars().nth(4) == Some('-') && ts.chars().nth(10) == Some('T')),
            "unexpected timestamp shape: {ts:?}"
        );
    }

    #[test]
    fn the_timestamp_has_millis_and_a_numeric_offset() {
        // The column is 29 bytes: `YYYY-MM-DDTHH:MM:SS.mmm+HH:MM`. A shorter one
        // would break `cut -c1-29` and the `journalctl` comparison in the docs.
        let ts = now_timestamp();
        if ts.starts_with("epoch=") {
            return; // Representable-range fallback; covered by the test above.
        }
        assert_eq!(ts.len(), 29, "timestamp must be RFC 3339 with millis and offset: {ts:?}");
        assert_eq!(ts.chars().nth(10), Some('T'), "{ts:?}");
        let offset = &ts[23..];
        assert!(
            offset.starts_with('+') || offset.starts_with('-'),
            "the offset must be numeric, not a timezone name: {ts:?}"
        );
        assert_eq!(offset.chars().nth(3), Some(':'), "{ts:?}");
    }

    #[test]
    fn tokens_never_contain_separators() {
        // Whitespace, `=`, `"` and `\` are what the reader splits on; a control
        // character would end the line.
        assert_eq!(token("weird event=name"), "weird_event_name");
        assert_eq!(token("bad key"), "bad_key");
        assert_eq!(token("tab\there"), "tab_here");
        assert_eq!(token("quote\"here"), "quote_here");
        assert_eq!(token("back\\slash"), "back_slash");
        assert_eq!(token("nl\nhere"), "nl_here");
        assert_eq!(token(""), "_", "an empty name must still yield a parseable field");
    }

    #[test]
    fn tokens_keep_ordinary_characters() {
        // The name is what a human greps for, so non-ASCII and punctuation that is
        // not a separator must survive verbatim.
        assert_eq!(token("tunnel.ready"), "tunnel.ready");
        assert_eq!(token("crash.panic.trace"), "crash.panic.trace");
        assert_eq!(token("配置-中文"), "配置-中文");
        assert_eq!(token("a:b,c"), "a:b,c");
    }

    #[test]
    fn plain_values_are_left_alone() {
        assert_eq!(value("abc"), "abc");
        assert_eq!(value("127.0.0.1:22"), "127.0.0.1:22");
        assert_eq!(value("中文值"), "中文值");
        assert_eq!(value("a=b"), "a=b", "`=` inside a value needs no quoting: only keys split on it");
    }

    #[test]
    fn ambiguous_values_are_quoted() {
        assert_eq!(value(""), "\"\"", "an empty value must not read as a missing field");
        assert_eq!(value("a b"), "\"a b\"");
        assert_eq!(value("leading "), "\"leading \"");
    }

    #[test]
    fn quotes_and_backslashes_are_escaped() {
        assert_eq!(value("say \"hi\""), "\"say \\\"hi\\\"\"");
        assert_eq!(value("c:\\tmp"), "\"c:\\\\tmp\"");
    }

    #[test]
    fn control_characters_are_escaped_so_one_record_stays_one_line() {
        assert_eq!(value("a\nb"), "\"a\\nb\"");
        assert_eq!(value("a\rb"), "\"a\\rb\"");
        assert_eq!(value("a\tb"), "\"a\\tb\"");
        // Anything else control-ish becomes `\xNN`, never a raw byte.
        assert_eq!(value("a\u{7}b"), "\"a\\x07b\"");
        for raw in ["a\nb", "a\rb", "a\u{0}b", "\u{1b}[0m"] {
            let rendered = value(raw);
            assert!(!rendered.contains('\n') && !rendered.contains('\r'), "{rendered:?} would split the record");
            assert_eq!(rendered.lines().count(), 1, "{rendered:?}");
        }
    }
}
