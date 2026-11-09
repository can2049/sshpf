//! Pure helpers behind the state machine: view construction, config repair and
//! reconnect backoff.
//!
//! These are the parts of `app` that need **no `App` at all** — no supervisor, no
//! channel, no form — which is why they live apart from the state machine proper:
//! they can be reasoned about (and tested) as plain functions.
//!
//! Two of them exist specifically to make hand-edited `config.toml` values safe
//! to use: [`sanitize_config`] clamps the retry defaults on load, and [`backoff`]
//! recomputes the same bounds so no arithmetic downstream can overflow or panic
//! on [`MIN_RETRY_SECS`]/[`MAX_RETRY_SECS`] input.

use std::time::{Duration, Instant};

use crate::config::ConfigFile;
use crate::eventlog;
use crate::model::{TunnelStatus, TunnelView};

use super::{MAX_RETRY_SECS, MIN_RETRY_SECS};

pub fn build_views(cfg: &ConfigFile) -> Vec<TunnelView> {
    cfg.tunnels
        .iter()
        .map(|c| {
            let mut v = TunnelView::new(c.clone());
            // Nothing is running until `restore_enabled` says so.
            v.status = TunnelStatus::Stopped;
            v
        })
        .collect()
}

/// Coerce hand-edited numeric defaults into the range the app can operate in.
///
/// Each field is clamped separately and any change is logged, so a config that
/// was quietly repaired is discoverable afterwards (`retry_initial_secs = 0`
/// would otherwise look like "sshpf reconnects in a tight loop for no reason").
pub fn sanitize_config(cfg: &mut ConfigFile) {
    let defaults = &mut cfg.defaults;
    let initial = defaults.retry_initial_secs.clamp(MIN_RETRY_SECS, MAX_RETRY_SECS);
    if initial != defaults.retry_initial_secs {
        eventlog::config_defaults_clamped("retry_initial_secs", defaults.retry_initial_secs, initial);
        defaults.retry_initial_secs = initial;
    }
    let max = defaults.retry_max_secs.clamp(MIN_RETRY_SECS, MAX_RETRY_SECS);
    if max != defaults.retry_max_secs {
        eventlog::config_defaults_clamped("retry_max_secs", defaults.retry_max_secs, max);
        defaults.retry_max_secs = max;
    }
    // `backoff` takes `max_secs.max(initial)` so an inverted pair is survivable,
    // but the cap should still be the larger of the two by the time it is used.
    if defaults.retry_max_secs < defaults.retry_initial_secs {
        let raised = defaults.retry_initial_secs;
        eventlog::config_defaults_clamped("retry_max_secs", defaults.retry_max_secs, raised);
        defaults.retry_max_secs = raised;
    }
    // `0` stays `0`: it is the documented "turn the check off" value, and
    // clamping it up would re-enable a feature the user disabled.
    let health = crate::config::normalize_health_check_secs(defaults.health_check_secs);
    if health != defaults.health_check_secs {
        eventlog::config_defaults_clamped("health_check_secs", defaults.health_check_secs, health);
        defaults.health_check_secs = health;
    }
}

/// Add `delay` to `now` without the panic `Instant + Duration` raises on
/// overflow.
///
/// `retry_max_secs` comes from a config file, and `Instant::checked_add` is the
/// only add that reports the overflow instead of aborting the process.
pub(crate) fn deadline_after(now: Instant, delay: Duration) -> Instant {
    match now.checked_add(delay) {
        Some(at) => at,
        None => now + Duration::from_secs(MIN_RETRY_SECS),
    }
}

/// Exponential backoff: 2s, 4s, 8s … capped at `max_secs`.
///
/// Both numbers come from a hand-editable config file, so neither the shift nor
/// the multiply may overflow (`saturating_*`), and the cap is normalised rather
/// than `clamp`ed: `clamp` panics when `min > max`, which is exactly what an
/// `initial > MAX_RETRY_SECS` config would hand it.
pub fn backoff(attempt: u32, initial: u64, max_secs: u64) -> Duration {
    let initial = initial.clamp(MIN_RETRY_SECS, MAX_RETRY_SECS);
    let cap = max_secs.clamp(initial, MAX_RETRY_SECS);
    let shift = attempt.saturating_sub(1).min(20);
    let secs = initial.saturating_mul(1u64 << shift).min(cap);
    Duration::from_secs(secs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{MAX_HEALTH_CHECK_SECS, MIN_HEALTH_CHECK_SECS};

    fn cfg_with_health(secs: u64) -> ConfigFile {
        let mut cfg = ConfigFile::default();
        cfg.defaults.health_check_secs = secs;
        cfg
    }

    #[test]
    fn a_health_check_interval_is_repaired_only_when_it_is_unusable() {
        // Hand-edited values arrive as input, not invariants: a `1` would probe
        // every second, a `u64::MAX` would never probe, and both would look
        // like "the feature is broken" instead of "the config is odd".
        let mut too_fast = cfg_with_health(1);
        sanitize_config(&mut too_fast);
        assert_eq!(too_fast.defaults.health_check_secs, MIN_HEALTH_CHECK_SECS);

        let mut too_slow = cfg_with_health(u64::MAX);
        sanitize_config(&mut too_slow);
        assert_eq!(too_slow.defaults.health_check_secs, MAX_HEALTH_CHECK_SECS);

        // A usable value is left exactly as written.
        let mut fine = cfg_with_health(45);
        sanitize_config(&mut fine);
        assert_eq!(fine.defaults.health_check_secs, 45);
    }

    #[test]
    fn turning_the_health_check_off_survives_the_repair() {
        // `0` is documented as "off". Reparing it up to the minimum would
        // silently re-enable a feature the user disabled — a config that does
        // not mean what it says is worse than no config.
        let mut off = cfg_with_health(0);
        sanitize_config(&mut off);
        assert_eq!(off.defaults.health_check_secs, 0, "0 means off and must stay off");
    }
}
