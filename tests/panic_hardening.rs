//! Black-box regression tests for the panic-hardening pass: the CLI must survive
//! hostile **input**, and the reason must end up in the diagnostic log.
//!
//! Scope, deliberately narrow: `tests/logging.rs` already covers the log format,
//! the crash hooks and the three failures that are not "input" (no terminal, a
//! non-UTF-8 *flag*, a closed stdout). What is pinned here is the parsing
//! boundary — `~/.ssh/config`, `config.toml`, the config path and the logging
//! environment variables — driven through the real binary so that a regression
//! shows up as a process exit status rather than as a unit-test assertion.
//!
//! Three properties are asserted for every case:
//!
//! 1. `status.code()` is `Some(..)` — the child was **not killed by a signal**.
//!    A Rust panic in the default configuration exits 101; a panic that aborts,
//!    or a stack overflow, or a `SIGSEGV` inside a parser, does not exit at all
//!    as far as a shell is concerned. `code() == None` is the only trace of that
//!    class of bug, so it is checked on its own line in [`exit_code`].
//! 2. The exit code is in the range the mode can legitimately produce (0 =
//!    handled, 1 = reported failure, 2 = usage error).
//! 3. The log contains the record that explains the case, and **no**
//!    `crash.panic` — "it did not crash" and "it said what happened" are one
//!    property, not two.
//!
//! Each test's doc comment names the pre-fix behaviour it pins. Those were
//! reproduced against the `ce3e933` binary (the commit before this branch), so
//! "this test would fail on the old tree" is measured, not assumed.

use std::path::Path;
use std::process::{Command, Output};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_sshpf")
}

/// A child process with an isolated log directory and config path.
///
/// Both are per-process environment variables, so every test gets its own
/// `tempdir` instead of sharing the developer's real `~/.local/state/sshpf`.
fn cmd(log_dir: &Path, config: &Path) -> Command {
    let mut c = Command::new(bin());
    c.env("SSHPF_LOG_DIR", log_dir);
    c.env("SSHPF_CONFIG", config);
    c
}

/// Exit code of a child that must not have died from a signal.
///
/// `ExitStatus::code()` returns `None` exactly when the process was terminated
/// by a signal, so this is the one place where "it crashed at the C level"
/// (stack overflow, abort, SIGSEGV in a dependency) is turned into a readable
/// failure. Everything below asserts through this helper.
fn exit_code(out: &Output, what: &str) -> i32 {
    match out.status.code() {
        Some(code) => code,
        None => {
            #[cfg(unix)]
            {
                use std::os::unix::process::ExitStatusExt;
                if let Some(signal) = out.status.signal() {
                    panic!("{what}: 被信号 {signal} 杀死（不是正常退出）: {}", tail(out));
                }
            }
            panic!("{what}: 没有退出码（进程被信号杀死）: {}", tail(out));
        }
    }
}

/// Child output, folded into a failure message.
fn tail(out: &Output) -> String {
    format!("stdout={:?} stderr={:?}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
}

/// Records written by the run, also asserting the file stayed one-record-per-line.
///
/// A hostile value that reached the writer unescaped would merge two records or
/// break `grep`; checking parseability on every read makes that a failure here
/// instead of a surprise while reading a user's log.
fn records(log_dir: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(log_dir.join("sshpf.log")).unwrap_or_default();
    let lines: Vec<String> = text.lines().map(str::to_owned).collect();
    for line in &lines {
        assert_eq!(line.matches("event=").count(), 1, "每行必须恰好是一条记录: {line:?}");
    }
    lines
}

fn record<'a>(records: &'a [String], event: &str) -> Option<&'a str> {
    let needle = format!("event={event}");
    records.iter().find(|line| line.contains(&needle)).map(String::as_str)
}

/// Field of a record, e.g. `event=` / `msg=` (`tests/logging.rs` style: split on
/// spaces, so a quoted value keeps its quotes).
fn field<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let needle = format!("{key}=");
    line.split(' ').find_map(|f| f.strip_prefix(needle.as_str()))
}

fn joined(records: &[String]) -> String {
    records.join("\n")
}

/// The one outcome no input may ever produce.
fn assert_no_panic(records: &[String], what: &str) {
    assert!(record(records, "crash.panic").is_none(), "{what}: 触发了 panic: {}", joined(records));
}

/// The record that must exist, for a readable failure message.
fn require<'a>(records: &'a [String], event: &str, what: &str) -> &'a str {
    record(records, event).unwrap_or_else(|| panic!("{what}: 日志里没有 event={event}: {}", joined(records)))
}

// ---------------------------------------------------------------------------
// ~/.ssh/config: hostile text
// ---------------------------------------------------------------------------

/// The headline case: a config whose first line is a multi-byte comment.
///
/// Pre-fix this exited **101** with
/// `end byte index 5 is not a char boundary; it is inside '文'` — `strip_match_blocks`
/// sliced the trimmed line at `[..5]` to compare the `Match` keyword, and byte 5
/// lands inside a CJK character on the very first line. Any non-ASCII comment at
/// the top of the file (a Chinese note, an accented hostname, an emoji) hit it.
///
/// The test fails on the old tree at the exit-code assertion (101 ∉ {0,1}) and
/// additionally proves the fix did not just swallow the file: the host below the
/// comment is still listed.
#[test]
fn a_multibyte_first_line_in_the_ssh_config_does_not_panic() {
    let dir = tempfile::tempdir().unwrap();
    let ssh_config = dir.path().join("ssh_config");
    std::fs::write(&ssh_config, "#中文注释\nHost demo\n  HostName 10.0.0.1\n").unwrap();

    let out = cmd(dir.path(), &dir.path().join("config.toml"))
        .env("SSHPF_SSH_CONFIG", &ssh_config)
        .arg("--list-hosts")
        .output()
        .unwrap();

    let code = exit_code(&out, "中文首行注释");
    assert_eq!(code, 0, "一个普通的中文注释不该让 --list-hosts 失败: {}", tail(&out));

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("demo"), "注释后面的主机必须仍被列出: {stdout:?}");

    let log = records(dir.path());
    assert_no_panic(&log, "中文首行注释");
}

/// A *bare* multi-byte line (no `#`) is not a panic either — it is simply not a
/// directive, so the parse fails and that failure is recorded.
///
/// Pre-fix: exit 101, same `char boundary` panic, because the slice happens
/// before the line is classified as anything.
#[test]
fn a_bare_multibyte_line_is_a_recorded_load_failure_not_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    let ssh_config = dir.path().join("ssh_config");
    std::fs::write(&ssh_config, "中文注释\nHost demo\n  HostName 10.0.0.1\n").unwrap();

    let out = cmd(dir.path(), &dir.path().join("config.toml"))
        .env("SSHPF_SSH_CONFIG", &ssh_config)
        .arg("--list-hosts")
        .output()
        .unwrap();

    let code = exit_code(&out, "裸中文行");
    assert_eq!(code, 1, "解析失败应以 1 退出，而不是 101: {}", tail(&out));
    // The user still learns why, on stderr and in the log.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("读取 ~/.ssh/config 失败"), "原因要到达 stderr: {stderr:?}");

    let log = records(dir.path());
    assert_no_panic(&log, "裸中文行");
    let failed = require(&log, "sshconfig.load_failed", "裸中文行");
    assert!(failed.contains(" ERROR "), "读不进来是错误级: {failed}");
    assert!(field(failed, "msg").is_some(), "记录要带原因: {failed}");
}

/// Fixing the byte slice must not lose the *other* half of the behaviour:
/// `strip_match_blocks` exists so a `Match` block's contents never leak into the
/// preceding `Host` block. A `Match` line containing multi-byte text is the
/// place where a careless fix (char-based comparison, `trim_start_matches`)
/// breaks one property while repairing the other.
///
/// `hidden` is declared inside a `Match` block and must not be listed; `after`
/// follows the block and must be.
#[test]
fn a_match_block_with_multibyte_text_is_still_fully_stripped() {
    let dir = tempfile::tempdir().unwrap();
    let ssh_config = dir.path().join("ssh_config");
    std::fs::write(
        &ssh_config,
        "# 注释\nHost demo\n  HostName 10.0.0.1\nMatch host demo 中English\n  Host hidden\n  HostName 9.9.9.9\nHost after\n  HostName 8.8.8.8\n",
    )
    .unwrap();

    let out = cmd(dir.path(), &dir.path().join("config.toml"))
        .env("SSHPF_SSH_CONFIG", &ssh_config)
        .arg("--list-hosts")
        .output()
        .unwrap();

    assert_eq!(exit_code(&out, "含中文的 Match 块"), 0, "{}", tail(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("demo") && stdout.contains("after"), "块外的主机要保留: {stdout:?}");
    assert!(!stdout.contains("hidden"), "Match 块里的 Host 不能被当成真主机: {stdout:?}");

    let log = records(dir.path());
    assert_no_panic(&log, "含中文的 Match 块");
}

/// Content shapes that are legal-ish in a hand-written config but hostile to a
/// line-oriented parser: a NUL byte, an unclosed quote, invalid UTF-8, a 150 KB
/// line, 300 `Include`s, and an empty file.
///
/// Pre-fix, only the multi-byte cases above panicked; the rest were already
/// survivable. They are pinned here because "the parser is fed arbitrary bytes
/// from a file the user may have generated with a script" is the invariant of
/// this whole pass, and each row is one way to violate a different assumption
/// (C-string truncation, quote tracking, UTF-8 validity, line buffer size, queue
/// size, empty input).
#[test]
fn hostile_ssh_configs_are_survived_with_a_normal_exit_code() {
    const LONG_LINE: usize = 150_000;

    fn include_flood() -> Vec<u8> {
        let mut body = String::from("Host base\n  HostName 1.2.3.4\n");
        for i in 0..300 {
            body.push_str(&format!("Include /nonexistent/sshpf-test-{i}.conf\n"));
        }
        body.into_bytes()
    }

    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("含 NUL 字节", b"Host a\x00b\n  HostName 1.2.3.4\n".to_vec()),
        ("未闭合引号", b"Host q\n  LocalForward \"8080 127.0.0.1:80\n".to_vec()),
        ("非 UTF-8 字节", b"Host \xffbad\n  HostName 1.2.3.4\n".to_vec()),
        ("150 KB 单行", {
            let mut body = b"Host big\n  LocalForward 8080 ".to_vec();
            body.extend(std::iter::repeat_n(b'a', LONG_LINE));
            body.extend_from_slice(b":80\n");
            body
        }),
        ("300 条 Include", include_flood()),
        ("空文件", Vec::new()),
    ];

    for (what, body) in cases {
        let dir = tempfile::tempdir().unwrap();
        let ssh_config = dir.path().join("ssh_config");
        std::fs::write(&ssh_config, &body).unwrap();

        let out = cmd(dir.path(), &dir.path().join("config.toml"))
            .env("SSHPF_SSH_CONFIG", &ssh_config)
            .arg("--list-hosts")
            .output()
            .unwrap();

        let code = exit_code(&out, what);
        assert!(code == 0 || code == 1, "{what}: 只能是「已处理」或「已报告」，实际 {code}: {}", tail(&out));

        let log = records(dir.path());
        assert_no_panic(&log, what);
        // Every run reaches the start record, so an empty log would mean the
        // logger itself gave up on this input.
        require(&log, "app.start", what);
        if code == 1 {
            require(&log, "sshconfig.load_failed", what);
        }
    }
}

// ---------------------------------------------------------------------------
// config.toml: hand-edited values
// ---------------------------------------------------------------------------

/// A `config.toml` that does not parse must be a recorded error (exit 1), never
/// a panic: it is a text file the user is explicitly allowed to edit.
///
/// The rows are chosen for the type-level corners a person actually hits:
/// plain garbage, a version from the future, a version as a string, a port that
/// does not fit in `u16`, an `id` as a string, and a file that is not UTF-8 at
/// all (a UTF-16 editor save, or `printf` with a wrong locale).
#[test]
fn a_broken_config_toml_is_a_recorded_error_not_a_panic() {
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("不是 TOML", b"this is not toml [[[\n".to_vec()),
        ("版本高于本程序", b"version = 999\n".to_vec()),
        ("版本类型错误", b"version = \"1\"\n".to_vec()),
        ("端口超出 u16", b"version = 1\n[[tunnels]]\nid = 1\nname = \"a\"\nssh_host = \"d\"\nlocal_port = 70000\nremote_port = 80\n".to_vec()),
        ("id 是字符串", b"version = 1\n[[tunnels]]\nid = \"notanumber\"\nname = \"a\"\nssh_host = \"d\"\nlocal_port = 8080\nremote_port = 80\n".to_vec()),
        ("非 UTF-8 字节", b"version = 1\n[[tunnels]]\nid = 1\nname = \"\xff\xfe\"\nssh_host = \"d\"\nlocal_port = 8080\nremote_port = 80\n".to_vec()),
    ];

    for (what, body) in cases {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        std::fs::write(&config, &body).unwrap();

        let out = cmd(dir.path(), &config).arg("--check").output().unwrap();

        let code = exit_code(&out, what);
        assert_eq!(code, 1, "{what}: 坏配置应以 1 退出: {}", tail(&out));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("配置错误"), "{what}: 原因要到达 stderr: {stderr:?}");

        let log = records(dir.path());
        assert_no_panic(&log, what);
        assert!(record(&log, "config.invalid").is_some(), "{what}: 要留下 config.invalid: {}", joined(&log));
    }
}

/// A hand-written `id = 18446744073709551615` is legal TOML and must load.
///
/// The panic it used to cause was in `next_id` (`max_id() + 1` overflow, debug
/// builds), which is only reached when a tunnel is *created* — not reachable from
/// `--check`. What this test pins is the loading half: such a file is accepted and
/// repaired rather than rejected or crashing, and the config reports one tunnel.
/// The allocation half is pinned by `config::tests` in the library
/// (`next_id`/`unique_name`); see the note in `docs/logging.md`.
#[test]
fn a_hand_edited_huge_tunnel_id_does_not_break_loading() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(
        &config,
        // `created_at` is set so the only unusual thing about this file is the id.
        "version = 1\n[[tunnels]]\nid = 18446744073709551615\nname = \"maxed\"\nssh_host = \"d\"\nlocal_port = 8080\nremote_port = 80\ncreated_at = 1700000000\n",
    )
    .unwrap();

    let out = cmd(dir.path(), &config).arg("--check").output().unwrap();

    assert_eq!(exit_code(&out, "id = u64::MAX"), 0, "{}", tail(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("1 条转发"), "这一条必须被读进来: {stdout:?}");
    assert!(stdout.contains("校验通过"), "u64::MAX 的 id 本身不是校验错误: {stdout:?}");

    let log = records(dir.path());
    assert_no_panic(&log, "id = u64::MAX");
    assert!(record(&log, "config.invalid").is_none(), "不该被当成坏配置: {}", joined(&log));
}

/// Port `0` and `65535` are the two ends where a `u16` field is ambiguous:
/// `0` is a real validation error in this program (it never means "any port"),
/// `65535` is a perfectly good port. Neither may be a hard failure.
///
/// `--check` reports field problems as warnings and still exits 0 — that contract
/// is what lets a user run it in a pre-commit hook without the file being
/// unloadable. The old tree behaved the same way; the value of this test is that
/// it fails if a boundary ever becomes a `panic!`/`unwrap` in validation or if
/// `--check` starts turning warnings into a non-zero exit.
#[test]
fn boundary_ports_are_validated_without_a_panic() {
    fn config_with_local_port(port: u16) -> String {
        format!(
            "version = 1\n[[tunnels]]\nid = 7\nname = \"edge\"\nssh_host = \"d\"\nlocal_port = {port}\nremote_port = 80\n"
        )
    }

    // 0: a warning, exit 0, and the reason is in the log.
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(&config, config_with_local_port(0)).unwrap();
    let out = cmd(dir.path(), &config).arg("--check").output().unwrap();
    assert_eq!(exit_code(&out, "端口 0"), 0, "{}", tail(&out));
    let log = records(dir.path());
    assert_no_panic(&log, "端口 0");
    let invalid = require(&log, "config.invalid", "端口 0");
    assert!(invalid.contains("本地端口不能为 0"), "要说明是端口问题: {invalid}");

    // 65535: legal, so no warning at all.
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    std::fs::write(&config, config_with_local_port(u16::MAX)).unwrap();
    let out = cmd(dir.path(), &config).arg("--check").output().unwrap();
    assert_eq!(exit_code(&out, "端口 65535"), 0, "{}", tail(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("校验通过"), "65535 是合法端口: {stdout:?}");
    let log = records(dir.path());
    assert_no_panic(&log, "端口 65535");
    assert!(record(&log, "config.invalid").is_none(), "合法端口不该报警告: {}", joined(&log));
}

/// A 64 KiB name is what a pasted log line looks like when it lands in the wrong
/// field. It is ugly but valid, so it must load — the renderer, not the loader,
/// is what has to cope with the width.
#[test]
fn a_64k_tunnel_name_is_validated_without_a_panic() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config.toml");
    let name = "名".repeat(64 * 1024);
    std::fs::write(
        &config,
        format!(
            "version = 1\n[[tunnels]]\nid = 1\nname = \"{name}\"\nssh_host = \"d\"\nlocal_port = 8080\nremote_port = 80\ncreated_at = 1700000000\n"
        ),
    )
    .unwrap();

    let out = cmd(dir.path(), &config).arg("--check").output().unwrap();

    assert_eq!(exit_code(&out, "64K 名称"), 0, "{}", tail(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("1 条转发") && stdout.contains("校验通过"), "{stdout:?}");
    let log = records(dir.path());
    assert_no_panic(&log, "64K 名称");
    assert!(record(&log, "config.invalid").is_none(), "{}", joined(&log));
}

/// A directory where the config file should be (a typo on `-c`, or a mount that
/// disappeared) is a recorded read error, not a crash.
#[test]
fn a_directory_as_the_config_file_is_a_recorded_error() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config-dir");
    std::fs::create_dir(&config).unwrap();

    let out = cmd(dir.path(), &config).arg("--check").output().unwrap();

    assert_eq!(exit_code(&out, "配置路径是目录"), 1, "{}", tail(&out));
    let log = records(dir.path());
    assert_no_panic(&log, "配置路径是目录");
    let invalid = require(&log, "config.invalid", "配置路径是目录");
    assert!(invalid.contains("读取配置失败"), "要说明是读不出来: {invalid}");
}

/// A non-UTF-8 `-c` path used to panic inside `std::env::args()` — that function
/// unwraps every argument as UTF-8. A path is an OS string, so it has to survive
/// byte-for-byte; `main` reads `args_os()` and keeps the path as an `OsString`.
///
/// `tests/logging.rs` covers the same bug for a non-UTF-8 *flag* (which must be a
/// usage error). This is the other half: in path position the bytes are valid
/// input, so the run must succeed.
#[cfg(unix)]
#[test]
fn a_non_utf8_config_path_is_kept_verbatim() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let dir = tempfile::tempdir().unwrap();
    let raw_path = dir.path().join(OsString::from_vec(b"conf\xff.toml".to_vec()));
    std::fs::write(&raw_path, "version = 1\n").unwrap();

    let out = cmd(dir.path(), &dir.path().join("unused.toml"))
        .arg("--config")
        .arg(raw_path.as_os_str())
        .arg("--check")
        .output()
        .unwrap();

    // 101 is the panic exit code, and the old tree's `code()` for this case.
    let code = exit_code(&out, "非 UTF-8 配置路径");
    assert_eq!(code, 0, "路径里的非 UTF-8 字节是合法输入: {}", tail(&out));
    let log = records(dir.path());
    assert_no_panic(&log, "非 UTF-8 配置路径");
    require(&log, "app.start", "非 UTF-8 配置路径");
}

// ---------------------------------------------------------------------------
// environment variables
// ---------------------------------------------------------------------------

/// `SSHPF_LOG_LEVEL` is free-form user input; a typo must degrade to `info` and
/// say so. The values here are the ones that are *not* plain ASCII letters —
/// a Chinese word, an empty string, a value with a trailing space, and a
/// lookalike with a stray character — because those are what a byte-oriented
/// `parse` implementation trips over, and because the record has to stay a
/// single line even when the rejected value is not ASCII.
///
/// Pre-fix this already degraded correctly; it is pinned so a future `Level::parse`
/// cannot start slicing the value or rejecting it with a panic.
#[test]
fn a_non_ascii_log_level_is_recorded_and_falls_back() {
    for level in ["banana", "警告", "", "info\u{200b}", "INFOO"] {
        let dir = tempfile::tempdir().unwrap();
        let out = cmd(dir.path(), &dir.path().join("config.toml"))
            .env("SSHPF_LOG_LEVEL", level)
            .arg("--check")
            .output()
            .unwrap();

        let code = exit_code(&out, level);
        assert_eq!(code, 0, "级别写错不是致命错误（{level:?}）: {}", tail(&out));

        let log = records(dir.path());
        assert_no_panic(&log, level);
        let invalid = require(&log, "log.level_invalid", level);
        assert!(invalid.contains(" WARN "), "回退是一次警告: {invalid}");
        // Falling back to `info` means the normal start record is still written.
        require(&log, "app.start", level);
    }
}

/// A log directory that cannot be created must not fail the run: logging is a
/// diagnostic aid, and refusing to start because the *log* is unwritable would be
/// exactly backwards. `/proc` is a read-only kernel filesystem on Linux; on other
/// unixes the same path simply does not exist, which is the same failure mode
/// (the directory cannot be created).
#[cfg(unix)]
#[test]
fn a_log_dir_that_cannot_be_created_does_not_fail_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let out = cmd(Path::new("/proc/nonexistent/sshpf-logs"), &dir.path().join("config.toml"))
        .arg("--check")
        .output()
        .unwrap();

    assert_eq!(exit_code(&out, "日志目录不可写"), 0, "日志失败不致命: {}", tail(&out));
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("校验通过"), "校验结果照常输出: {stdout:?}");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(!stderr.contains("panicked"), "不能以 panic 的形式失败: {stderr:?}");
}

/// `--help` and `--version` write to stdout with `print!`/`println!` in the old
/// tree, and those **abort** (panic) when the write fails: `sshpf --help | head
/// -1` or `> /dev/full`. Both now go through an explicit handle whose error is
/// ignored, so the exit code still reports what happened.
///
/// `tests/logging.rs` pins the same bug for `--list-hosts`; this covers the two
/// paths that run *before* the log is even opened, i.e. the ones with the least
/// fallback.
#[cfg(unix)]
#[test]
fn help_and_version_that_cannot_be_written_do_not_panic() {
    let dev_full = Path::new("/dev/full");
    if !dev_full.exists() {
        return; // Linux-specific sink; the closed-pipe variant is in tests/logging.rs.
    }

    for flag in ["--help", "--version"] {
        let dir = tempfile::tempdir().unwrap();
        let sink = std::fs::OpenOptions::new().write(true).open(dev_full).unwrap();
        let out = Command::new(bin())
            .env("SSHPF_LOG_DIR", dir.path())
            .env("SSHPF_CONFIG", dir.path().join("config.toml"))
            .arg(flag)
            .stdout(std::process::Stdio::from(sink))
            .output()
            .unwrap();

        let code = exit_code(&out, flag);
        assert_ne!(code, 101, "{flag}: 写不出 stdout 不能是 panic: {}", tail(&out));
        assert_eq!(code, 0, "{flag}: 帮助/版本文本发不出去不算失败: {}", tail(&out));
    }
}
