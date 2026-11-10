//! The argument surface itself: one `clap` derive struct, and the mapping from
//! `clap`'s errors back to this program's two usage messages.
//!
//! # Why clap now
//!
//! The parser here used to be a hand-written `while let` over `args_os()`. It was
//! ~35 lines and correct, but every rule (which flags exist, which take a value,
//! what a duplicate means) lived in a `match` arm that no compiler checked
//! against the struct's fields. `clap` makes the struct the single declaration:
//! a flag that is missing from the struct does not exist, and `--help`/`--version`
//! are declared once instead of being special-cased in two arms. See
//! `docs/design.md` D38 for the trade-off (5 packages, and why that is acceptable
//! where D37's rule says otherwise).
//!
//! # What does *not* come from clap
//!
//! * **The help text.** [`super::USAGE`] stays the single source of truth and is
//!   printed by hand. It is a 111-line bilingual manual with free-standing
//!   sections (key bindings, mouse, logging) that no derive attribute can hold,
//!   and its wording is load-bearing: `docs/keymap.md` and the README mirror it,
//!   `tests/help_coverage.rs` cross-checks parts of it. Letting clap render help
//!   would rewrite all of that for no gain.
//! * **The error wording.** clap's own messages are English, `error:`-prefixed
//!   and include a `Usage:` block the manual help does not have. [`classify`]
//!   turns each `ErrorKind` back into the exact string this program has always
//!   printed, so the exit codes (2) and the text stay byte-identical.
//! * **The write path.** Nothing here prints. Help and version go through
//!   [`super::write_stdout`] so a broken pipe or a full disk cannot panic.
//!
//! # Three places where clap's behaviour wins over the old hand-written loop
//!
//! The parser this replaced accepted things clap rejects, and vice versa. Where
//! they disagree, **clap's rule is the rule** — the shims that used to paper over
//! the difference are gone on purpose, and the new behaviour is pinned by tests in
//! `src/cli/tests.rs` so it is a decision rather than drift:
//!
//! * **A repeated flag is an error.** `sshpf --check --check` now exits 2 with
//!   clap's "cannot be used multiple times". The old loop silently set the bool
//!   twice; a flag given twice is a mistake worth reporting.
//! * **`--config` needs a value that is not flag-shaped.** `sshpf -c --help` now
//!   fails with "--config needs a path argument" (exit 2) instead of storing
//!   `--help` as the config path. A path that starts with `--` has to be written
//!   as `--config=--help` if someone really means it.
//! * **An attached value is accepted.** `--config=PATH` / `-cPATH` work; the old
//!   loop only matched the exact tokens `-c` / `--config`.
//!
//! Belongs here: the flag declarations, and the error→message mapping.
//! Does not belong: the help text ([`super::USAGE`]), the non-TUI modes
//! ([`super::list_hosts`] / [`super::check_config`]), or the path layout
//! ([`super::paths_for`]).

use std::ffi::OsString;
use std::io::Write as _;

use clap::Parser as _;

/// What the command line asked for.
///
/// Public so `main` can read the fields without a getter per flag; the struct is
/// inert data. The field order here is the order `--help` would print in, which
/// is why the flags that take a value and the mode flags are not interleaved.
#[derive(Debug, Default, clap::Parser)]
#[command(
    name = "sshpf",
    version,
    about = "SSH 端口转发 TUI 管理器 / SSH port-forward manager for the terminal",
    // `USAGE` is printed by `cli.rs`, never assembled by clap: it is longer and
    // more structured than any derive attribute can express.
    disable_help_flag = true,
    disable_version_flag = true
)]
pub struct Args {
    /// Kept as an `OsString`: a config path need not be valid UTF-8.
    ///
    /// The value must not look like a flag: `-c --help` is a usage error, not a
    /// file called `--help`. Write `--config=--help` to mean the path.
    #[arg(short = 'c', long, value_name = "PATH")]
    pub config: Option<OsString>,
    /// Start without restoring the forwards that were enabled in the last run.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub no_restore: bool,
    /// Print the hosts from `~/.ssh/config`, then exit.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub list_hosts: bool,
    /// Validate the config file, then exit (no UI).
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub check: bool,
    /// Accept a changed host key instead of refusing the connection.
    #[arg(long, action = clap::ArgAction::SetTrue)]
    pub ignore_host_key_changes: bool,
    /// Declared only so the flag parses; `cli.rs` prints the real help.
    #[arg(short = 'h', long, action = clap::ArgAction::Help)]
    help: Option<bool>,
    /// Declared only so the flag parses; `cli.rs` prints the real version.
    #[arg(short = 'V', long, action = clap::ArgAction::Version)]
    version: Option<bool>,
}

impl Args {
    /// Process-wide host-key policy implied by the command line.
    pub fn host_key_policy(&self) -> crate::ssh::known_hosts::HostKeyPolicy {
        if self.ignore_host_key_changes {
            crate::ssh::known_hosts::HostKeyPolicy::Ignore
        } else {
            crate::ssh::known_hosts::HostKeyPolicy::default()
        }
    }
}

/// Parse `args` (without the program name).
///
/// `Ok(None)` means help or version was asked for and has already been written;
/// `Ok(Some(_))` is a normal run; `Err(_)` is a usage error, already worded the
/// way the program has always worded it.
pub fn parse<I>(args: I) -> Result<Option<Args>, String>
where
    I: IntoIterator<Item = OsString>,
{
    // clap wants argv[0] present; the real one is irrelevant (it is only used in
    // the usage line of clap's own errors, which `classify` discards).
    let argv = std::iter::once(OsString::from("sshpf")).chain(args);
    match Args::try_parse_from(argv) {
        Ok(parsed) => Ok(Some(parsed)),
        Err(e) => classify(e),
    }
}

/// Turn a `clap` error into this program's own message, or into "handled".
///
/// Three outcomes, and the distinction matters:
///
/// * help/version — print through [`super::write_stdout`] and return `Ok(None)`,
///   because "the user asked what this program does" is success, not failure;
/// * a usage error — return the bilingual sentence the old parser printed, so
///   `tests/logging.rs` and `tests/panic_hardening.rs` keep their exit-code-2
///   contract;
/// * anything else — fall back to clap's text, which is still a sane message and
///   never a panic.
///
/// `try_parse_from` is deliberate: `Args::parse()` calls `e.exit()`, which is
/// `process::exit` — it would skip every `Drop` on the way out (tunnels stopped,
/// terminal restored). The clap docs' `err.print().expect(..)` is likewise out:
/// `.expect` is a panic, and `--help > /dev/full` must not panic.
fn classify(e: clap::Error) -> Result<Option<Args>, String> {
    use clap::error::{ContextKind, ErrorKind};
    match e.kind() {
        // The two "asked a question, answered it" cases. `write_stdout` reports a
        // broken pipe instead of dying, and the exit code stays 0 either way.
        ErrorKind::DisplayHelp => {
            let _ = super::write_stdout(super::USAGE);
            Ok(None)
        }
        ErrorKind::DisplayVersion => {
            let _ = writeln!(std::io::stdout(), "sshpf {}", env!("CARGO_PKG_VERSION"));
            Ok(None)
        }
        // A flag that is not in the struct. The bad token is recovered so the
        // message can name it, exactly as the hand-written parser did.
        ErrorKind::UnknownArgument => {
            let bad = context_string(&e, ContextKind::InvalidArg);
            Err(format!("未知参数 / Unknown option: {bad}\n\n{}", super::USAGE))
        }
        // `--config` with nothing after it. clap reports either a missing value
        // or an invalid one depending on where the argument list ends, so both
        // collapse to the one sentence this program has always printed.
        ErrorKind::InvalidValue | ErrorKind::ValueValidation => {
            Err("--config 需要一个路径参数 / --config needs a path argument".to_string())
        }
        ErrorKind::MissingRequiredArgument => {
            Err("--config 需要一个路径参数 / --config needs a path argument".to_string())
        }
        // A flag given twice (`--check --check`). clap's wording is English and
        // carries a `Usage:` block the manual help does not have, so it gets the
        // same bilingual treatment as the other usage errors.
        ErrorKind::ArgumentConflict => {
            let bad = context_string(&e, ContextKind::InvalidArg);
            Err(format!("参数不能重复 / Option given more than once: {bad}\n\n用法 / Usage:  sshpf [选项 / options]"))
        }
        // A value given to a flag that takes none (`--check=1`). The old parser
        // never got here — it treated the whole token as an unknown option — so
        // there is no historic sentence to reproduce; this one is new, and it is
        // worded the way the rest of the program words things (bilingual, no
        // `error:` prefix, exit 2).
        ErrorKind::TooManyValues => {
            Err("该选项不接受取值 / This option takes no value\n\n用法 / Usage:  sshpf [选项 / options]".to_string())
        }
        // Any other kind is genuinely clap's to describe (a value given to a flag
        // that takes none, say). Showing clap's own explanation is more truthful
        // than inventing a Chinese sentence for a case that should not occur;
        // `to_string` never panics.
        _ => Err(e.to_string()),
    }
}

/// One string out of clap's structured error context, or `""` if absent.
///
/// Returning an empty string rather than an `Option` keeps the call site free of
/// `unwrap_or_default` noise while still never panicking.
fn context_string(e: &clap::Error, key: clap::error::ContextKind) -> String {
    use clap::error::ContextValue;
    match e.context().find(|(k, _)| *k == key).map(|(_, v)| v) {
        Some(ContextValue::String(s)) => s.clone(),
        _ => String::new(),
    }
}
