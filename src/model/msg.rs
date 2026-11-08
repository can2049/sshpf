//! The transient status line, and the crate's two clock helpers.
//!
//! `StatusMsg` is what the user sees at the bottom of the screen after an
//! action: one line, a level to colour it by, and a timestamp so it fades. It is
//! model state, not rendering — the renderer only decides where the line goes.
//!
//! Belongs here: status text/level/lifetime, `HH:MM:SS` and unix timestamps.
//! Does not belong: anything that writes to the log file (`logging`).

use std::time::Instant;

use super::status::LogLevel;

/// Format the current wall clock as `HH:MM:SS` in local time.
pub fn now_hms() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// Unix timestamp right now.
pub fn now_unix() -> i64 {
    chrono::Local::now().timestamp()
}

/// Non-modal single-line message shown at the bottom of the screen.
#[derive(Debug, Clone)]
pub struct StatusMsg {
    pub text: String,
    pub level: LogLevel,
    pub at: Instant,
}

impl StatusMsg {
    pub fn info(text: impl Into<String>) -> Self {
        StatusMsg { text: text.into(), level: LogLevel::Info, at: Instant::now() }
    }

    pub fn ok(text: impl Into<String>) -> Self {
        StatusMsg { text: text.into(), level: LogLevel::Info, at: Instant::now() }
    }

    pub fn warn(text: impl Into<String>) -> Self {
        StatusMsg { text: text.into(), level: LogLevel::Warn, at: Instant::now() }
    }

    pub fn error(text: impl Into<String>) -> Self {
        StatusMsg { text: text.into(), level: LogLevel::Error, at: Instant::now() }
    }

    /// Messages fade out after ~4s.
    pub fn expired(&self) -> bool {
        self.at.elapsed().as_millis() > 4000
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_msg_expires() {
        let fresh = StatusMsg::info("hi");
        assert!(!fresh.expired(), "a just-created message is visible");
        let mut old = StatusMsg::ok("hi");
        old.at = Instant::now() - std::time::Duration::from_secs(10);
        assert!(old.expired(), "messages fade out");
        // All constructors carry a level the UI can colour by.
        assert_eq!(StatusMsg::error("e").level, LogLevel::Error);
        assert_eq!(StatusMsg::warn("w").level, LogLevel::Warn);
        assert_eq!(StatusMsg::ok("o").level, LogLevel::Info);
    }
}
