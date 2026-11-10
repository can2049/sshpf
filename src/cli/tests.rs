//! Unit tests for `cli.rs`.
//!
//! Moved out of line so the production module stays a readable map of what it
//! does. `#![cfg(test)]` keeps this file out of every non-test build, and tells
//! `tests/safety_guards.rs` not to count it against the production line budget.
#![cfg(test)]

use super::*;
use std::ffi::OsString;

fn parse(args: &[&str]) -> Result<Option<Args>, String> {
    parse_args_from(args.iter().map(OsString::from))
}

/// The error half of a parse, for the cases that must fail.
fn parse_err(args: &[&str]) -> String {
    match parse(args) {
        Err(e) => e,
        Ok(_) => panic!("expected a usage error for {args:?}"),
    }
}

#[test]
fn plain_flags_parse() {
    let args = parse(&["--check"]).unwrap().unwrap();
    assert!(args.check && !args.list_hosts && !args.no_restore);
    let args = parse(&["--list-hosts", "--no-restore"]).unwrap().unwrap();
    assert!(args.list_hosts && args.no_restore && !args.check);
}

#[test]
fn config_takes_the_next_argument_verbatim() {
    let args = parse(&["-c", "/tmp/x.toml"]).unwrap().unwrap();
    assert_eq!(args.config.as_deref(), Some(std::ffi::OsStr::new("/tmp/x.toml")));
    let args = parse(&["--config", "rel/config.toml"]).unwrap().unwrap();
    assert_eq!(args.config.as_deref(), Some(std::ffi::OsStr::new("rel/config.toml")));
}

/// **Deliberate behaviour change** (was: accepted): a repeated flag is now a
/// usage error.
///
/// The old hand-written loop just set the bool a second time, so `--check --check`
/// silently worked. clap rejects the second occurrence, and that is the rule we
/// keep: a flag given twice is a mistake worth reporting, not something to absorb.
/// The message is bilingual like the other usage errors, so it does not leak
/// clap's English `error:`/`Usage:` block into a program that speaks Chinese
/// first.
#[test]
fn a_repeated_flag_is_a_usage_error() {
    for flag in ["--check", "--list-hosts", "--no-restore", "--ignore-host-key-changes"] {
        let err = parse_err(&[flag, flag]);
        assert!(err.contains("参数不能重复"), "{flag}: {err}");
        assert!(err.contains(flag.trim_start_matches('-')), "the message must name the flag: {err}");
    }
    // The flag with a value repeats the same way.
    let err = parse_err(&["-c", "/tmp/a.toml", "-c", "/tmp/b.toml"]);
    assert!(err.contains("参数不能重复"), "{err}");
}

/// **Deliberate behaviour change** (was: stored as the path): a flag-shaped value
/// for `--config` is now a usage error.
///
/// The old parser took whatever came next (`it.next()` with no inspection), so
/// `sshpf -c --help` tried to open a config file literally named `--help`. clap
/// refuses to treat a `--…` token as a value, which is the safer reading: a typo
/// like `-c --check` now says so instead of failing later with a confusing
/// "failed to read --check".
///
/// A path that really does start with `--` still works — write it attached.
#[test]
fn a_flag_shaped_config_value_is_a_usage_error() {
    let err = parse_err(&["--config", "--help"]);
    assert!(err.contains("--config"), "{err}");
    assert!(err.contains("需要一个路径参数"), "the message must say a path is missing: {err}");
    // The escape hatch for a genuinely dash-leading path.
    let args = parse(&["--config=--help"]).unwrap().unwrap();
    assert_eq!(args.config.as_deref(), Some(std::ffi::OsStr::new("--help")));
}

/// **Deliberate behaviour change**: `--config=PATH` and `-cPATH` are now accepted.
///
/// The old loop compared the whole token against `"-c"`/`"--config"`, so an
/// attached value fell through to "unknown option". clap implements the
/// conventional GNU form, and reproducing the rejection would mean fighting the
/// parser for no user benefit. Recorded here so the change is a decision rather
/// than a surprise, and so the CHANGELOG entry has a test behind it.
#[test]
fn an_attached_config_value_is_accepted() {
    let args = parse(&["--config=/tmp/x.toml"]).unwrap().unwrap();
    assert_eq!(args.config.as_deref(), Some(std::ffi::OsStr::new("/tmp/x.toml")));
    let args = parse(&["-c/tmp/x.toml"]).unwrap().unwrap();
    assert_eq!(args.config.as_deref(), Some(std::ffi::OsStr::new("/tmp/x.toml")));
}

#[test]
fn missing_config_value_is_a_usage_error_not_a_panic() {
    let err = parse_err(&["--config"]);
    assert!(err.contains("--config"), "{err}");
}

#[test]
fn unknown_flags_are_rejected_with_the_usage_text() {
    let err = parse_err(&["--nope"]);
    assert!(err.contains("未知参数"), "{err}");
    assert!(err.contains("用法"), "the message must include the usage: {err}");
}

#[test]
fn help_and_version_short_circuit() {
    assert!(parse(&["--help"]).unwrap().is_none());
    assert!(parse(&["-h"]).unwrap().is_none());
    assert!(parse(&["-V"]).unwrap().is_none());
    assert!(parse(&["--version"]).unwrap().is_none());
    // Everything after `--help` is irrelevant, and must not be validated.
    assert!(parse(&["--help", "--totally-bogus"]).unwrap().is_none());
}

/// The bug: `std::env::args()` panics on a non-UTF-8 argument. A config path
/// is an OS path, so it has to survive byte-for-byte.
#[test]
fn a_non_utf8_config_path_is_accepted_verbatim() {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::{OsStrExt, OsStringExt};
        let raw = OsString::from_vec(b"/tmp/\xff\xfe.toml".to_vec());
        let parsed = parse_args_from(vec![OsString::from("--config"), raw.clone()]).unwrap().unwrap();
        assert_eq!(parsed.config.as_deref().map(|c| c.as_bytes()), Some(&b"/tmp/\xff\xfe.toml"[..]));
    }
    // The portable check: at minimum it must not panic, on any platform.
    let raw = OsString::from("/tmp/config.toml");
    assert!(parse_args_from(vec![OsString::from("--config"), raw]).is_ok());
}

/// A non-UTF-8 *option name* cannot be a known flag, so it is a usage error
/// (exit 2) rather than a panic.
#[test]
fn a_non_utf8_flag_is_a_usage_error() {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStringExt;
        let raw = OsString::from_vec(b"--\xff".to_vec());
        let err = match parse_args_from(vec![raw]) {
            Err(e) => e,
            Ok(_) => panic!("a non-UTF-8 flag must be a usage error"),
        };
        assert!(err.contains("未知参数"), "{err}");
    }
}

#[test]
fn write_stdout_reports_instead_of_panicking() {
    // Not a tty here, so this exercises the buffered path end to end.
    assert!(write_stdout("sshpf test line\n"));
}

#[test]
fn ignore_host_key_changes_is_off_by_default() {
    let args = parse(&[]).unwrap().unwrap();
    assert!(!args.ignore_host_key_changes, "the escape hatch must be opt-in, never the default");
    assert_eq!(args.host_key_policy(), crate::ssh::known_hosts::HostKeyPolicy::AcceptNew);
}

#[test]
fn ignore_host_key_changes_flag_parses_to_the_ignore_policy() {
    let args = parse(&["--ignore-host-key-changes"]).unwrap().unwrap();
    assert!(args.ignore_host_key_changes);
    assert_eq!(args.host_key_policy(), crate::ssh::known_hosts::HostKeyPolicy::Ignore);
}

/// The flag is additive: it must not swallow the other modes, because
/// `--check --ignore-host-key-changes` is a legitimate "validate with my
/// usual flags" invocation.
#[test]
fn ignore_host_key_changes_composes_with_the_other_flags() {
    let args = parse(&["--check", "--ignore-host-key-changes", "--no-restore"]).unwrap().unwrap();
    assert!(args.check && args.no_restore && args.ignore_host_key_changes);
    assert_eq!(args.host_key_policy(), crate::ssh::known_hosts::HostKeyPolicy::Ignore);
    // Position must not matter.
    let reordered = parse(&["--ignore-host-key-changes", "--check"]).unwrap().unwrap();
    assert!(reordered.check && reordered.ignore_host_key_changes);
}

/// Order after `--help` is irrelevant and must not be validated — the same
/// rule the other flags follow.
#[test]
fn ignore_host_key_changes_after_help_is_never_reached() {
    assert!(parse(&["--help", "--ignore-host-key-changes"]).unwrap().is_none());
}

#[test]
fn the_usage_text_documents_the_ignore_flag() {
    // Users grep the help text and `docs/keymap.md` mirrors it; a flag with
    // no help line is undiscoverable.
    assert!(USAGE.contains("--ignore-host-key-changes"), "USAGE must list the flag");
    assert!(USAGE.contains("Accept a changed host key"), "the flag needs an English explanation");
}
