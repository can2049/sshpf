//! Unit tests for `select.rs`.
//!
//! Moved out of line so the production module stays a readable map of what it
//! does. `#![cfg(test)]` keeps this file out of every non-test build, and tells
//! `tests/safety_guards.rs` not to count it against the production line budget.
#![cfg(test)]

use super::*;
use crate::config::{ConfigFile, Paths};
use crate::model::{ForwardKind, TunnelConfig, TunnelStatus};
use std::time::Instant;

fn tunnel(id: crate::model::TunnelId, name: &str, port: u16) -> TunnelConfig {
    TunnelConfig {
        id,
        name: name.into(),
        kind: ForwardKind::Local,
        ssh_host: "h".into(),
        local_addr: "127.0.0.1".into(),
        local_port: port,
        remote_host: "127.0.0.1".into(),
        remote_port: port,
        enabled: false,
        auto_restart: false,
        ssh_options: vec![],
        note: String::new(),
        created_at: 1,
    }
}

fn app_with(tunnels: Vec<TunnelConfig>) -> (tempfile::TempDir, App) {
    let dir = tempfile::tempdir().unwrap();
    let paths = Paths { config: dir.path().join("config.toml"), state: dir.path().join("state.toml") };
    let cfg = ConfigFile { tunnels, ..ConfigFile::default() };
    crate::config::save(&paths.config, &cfg).unwrap();
    (dir, App::new(paths))
}

fn three() -> (tempfile::TempDir, App) {
    app_with(vec![tunnel(1, "a", 18001), tunnel(2, "b", 18002), tunnel(3, "c", 18003)])
}

#[test]
fn entering_selection_mode_starts_empty_but_keeps_the_cursor() {
    // Starting empty keeps `space` meaning "toggle the row under the cursor"
    // both before and after entering; pre-marking would make the first space
    // *unmark* the row the user just pointed at.
    let (_d, mut app) = three();
    app.selected = 1;
    app.toggle_select_mode();
    assert!(app.select_mode.active);
    assert!(app.select_mode.is_empty());
    assert_eq!(app.selected, 1, "the cursor stays where it was");
    assert_eq!(app.select_mode.anchor, Some(1), "the range anchor starts at the cursor");
    // The very next space marks it.
    app.toggle_mark_selected();
    assert_eq!(app.select_mode.ids(), vec![2]);
}

#[test]
fn space_toggles_a_mark_and_leaves_the_cursor_alone() {
    let (_d, mut app) = three();
    app.selected = 0;
    app.toggle_select_mode();
    app.toggle_mark_selected(); // mark row 1
    app.selected = 2;
    app.toggle_mark_selected();
    assert_eq!(app.select_mode.len(), 2, "{:?}", app.select_mode.ids());
    assert!(app.select_mode.is_marked(1) && app.select_mode.is_marked(3));
    assert_eq!(app.selected, 2, "marking must not move the cursor");
    // Toggling the same row again unmarks it.
    app.toggle_mark_selected();
    assert_eq!(app.select_mode.ids(), vec![1]);
}

#[test]
fn leaving_selection_mode_clears_every_mark() {
    let (_d, mut app) = three();
    app.toggle_select_mode();
    app.mark_all_visible();
    assert_eq!(app.select_mode.len(), 3);
    app.exit_select_mode();
    assert!(!app.select_mode.active);
    assert!(app.select_mode.is_empty(), "stale marks must not survive the mode");
}

#[test]
fn mark_all_covers_only_the_filtered_view() {
    // The footgun this pins: "select all" while a filter is active must not
    // mark rows the user cannot see, because the next batch delete would hit
    // them too.
    let (_d, mut app) = app_with(vec![tunnel(1, "alpha", 18001), tunnel(2, "beta", 18002)]);
    app.toggle_select_mode();
    app.filter = "alpha".into();
    app.mark_all_visible();
    assert_eq!(app.select_mode.ids(), vec![1], "beta is filtered out and must stay unmarked");
    app.filter.clear();
    app.mark_all_visible();
    assert_eq!(app.select_mode.len(), 2, "clearing the filter makes both selectable");
}

#[test]
fn mark_all_toggles_between_all_and_none() {
    let (_d, mut app) = three();
    app.toggle_select_mode();
    app.mark_all_toggle();
    assert_eq!(app.select_mode.len(), 3);
    app.mark_all_toggle();
    assert!(app.select_mode.is_empty(), "pressing a again unmarks everything");
}

#[test]
fn range_marking_follows_the_cursor_in_both_directions() {
    let (_d, mut app) = app_with((1..=5).map(|i| tunnel(i, &format!("t{i}"), 18000 + i as u16)).collect());
    app.toggle_select_mode();
    app.select_first();
    // Anchor the range at the first row, as entering the mode would.
    app.select_mode.anchor = Some(0);
    // Down three rows, extend: 1..=4.
    app.select_next();
    app.select_next();
    app.select_next();
    app.extend_mark_to_cursor();
    let ids = app.select_mode.ids();
    assert!(ids.contains(&1) && ids.contains(&4), "{ids:?}");
    // Now walk back up: the anchor moves with the extension, so the new
    // range must still be contiguous from the last anchor.
    app.select_prev();
    app.extend_mark_to_cursor();
    assert!(app.select_mode.len() >= 3);
}

#[test]
fn a_batch_request_with_no_marks_does_not_ask() {
    let (_d, mut app) = three();
    app.toggle_select_mode();
    app.mark_none();
    app.request_batch(BatchAction::Stop);
    assert!(matches!(app.overlay, Overlay::None), "asking to do nothing is noise");
}

#[test]
fn a_batch_request_on_already_stopped_tunnels_does_not_ask() {
    let (_d, mut app) = three();
    app.toggle_select_mode();
    app.mark_all_visible();
    // Everything is stopped, so "stop" would change nothing.
    app.request_batch(BatchAction::Stop);
    assert!(matches!(app.overlay, Overlay::None));
    // "start" would change something, so it asks.
    app.request_batch(BatchAction::Start);
    assert!(matches!(app.overlay, Overlay::Confirm { .. }));
}

#[test]
fn the_batch_prompt_counts_only_rows_that_change() {
    let (_d, mut app) = three();
    app.set_state(1, TunnelStatus::Up { since: Instant::now() });
    app.toggle_select_mode();
    app.mark_all_visible();
    app.request_batch(BatchAction::Stop);
    match &app.overlay {
        Overlay::Confirm { action: ConfirmAction::Batch { action, count }, prompt } => {
            assert_eq!(*action, BatchAction::Stop);
            assert_eq!(*count, 1, "only the running tunnel would stop");
            assert!(prompt.contains('1'), "prompt must state the effective count: {prompt}");
        }
        other => panic!("expected a batch confirmation, got {other:?}"),
    }
    // Nothing happened yet.
    assert!(matches!(app.tunnels[0].status, TunnelStatus::Up { .. }));
}

#[test]
fn confirming_a_batch_stop_stops_everything_marked() {
    let (_d, mut app) = three();
    for id in 1..=3 {
        app.set_state(id, TunnelStatus::Up { since: Instant::now() });
        if let Some(v) = app.tunnels.iter_mut().find(|t| t.cfg.id == id) {
            v.cfg.enabled = true;
        }
    }
    app.toggle_select_mode();
    app.mark_all_visible();
    app.request_batch(BatchAction::Stop);
    app.confirm_yes();
    assert!(app.tunnels.iter().all(|t| matches!(t.status, TunnelStatus::Stopped)), "{:?}", app.tunnels);
    assert!(app.tunnels.iter().all(|t| !t.cfg.enabled), "intent must be persisted as off");
    let reloaded = crate::config::load(&app.paths.config).unwrap();
    assert!(reloaded.tunnels.iter().all(|t| !t.enabled));
}

#[test]
fn cancelling_a_batch_changes_nothing() {
    let (_d, mut app) = three();
    app.set_state(1, TunnelStatus::Up { since: Instant::now() });
    app.toggle_select_mode();
    app.mark_all_visible();
    app.request_batch(BatchAction::Stop);
    app.close_overlay();
    assert!(matches!(app.tunnels[0].status, TunnelStatus::Up { .. }), "cancel must not stop anything");
    assert!(app.select_mode.active, "cancel keeps the mode and the marks");
}

#[test]
fn a_confirmed_batch_delete_removes_every_marked_tunnel() {
    let (_d, mut app) = three();
    app.toggle_select_mode();
    // Mark 1 and 3 only, leaving 2 alone.
    app.selected = 0;
    app.mark_none();
    app.select_first();
    app.toggle_mark_selected();
    app.select_last();
    app.toggle_mark_selected();
    app.request_batch(BatchAction::Delete);
    app.confirm_yes();
    let names: Vec<&str> = app.tunnels.iter().map(|t| t.cfg.name.as_str()).collect();
    assert_eq!(names, vec!["b"], "only the marked rows should be gone");
    let reloaded = crate::config::load(&app.paths.config).unwrap();
    assert_eq!(reloaded.tunnels.len(), 1, "deletion must be persisted");
    assert!(app.select_mode.is_empty(), "marks for deleted rows must be dropped");
}

#[test]
fn batch_delete_does_not_leave_a_dangling_cursor() {
    let (_d, mut app) = three();
    app.selected = 2;
    app.toggle_select_mode();
    app.mark_all_visible();
    app.request_batch(BatchAction::Delete);
    app.confirm_yes();
    assert!(app.tunnels.is_empty());
    assert_eq!(app.selected, 0, "the cursor must be clamped back into the empty view");
}

#[test]
fn marks_survive_a_non_delete_batch() {
    let (_d, mut app) = three();
    app.toggle_select_mode();
    app.mark_all_visible();
    app.request_batch(BatchAction::Start);
    app.confirm_yes();
    // The same set stays marked, so a follow-up action needs no re-selection.
    assert_eq!(app.select_mode.len(), 3, "marks should remain after starting");
}

#[test]
fn extending_a_range_in_an_empty_view_does_not_underflow() {
    // `extend_mark_to_cursor` used `ids.len() - 1` after checking only that
    // the app had tunnels; a filter that hides every row makes `ids` empty
    // while `self.tunnels` is not. That underflowed (panic in debug builds).
    let (_d, mut app) = three();
    app.toggle_select_mode();
    app.filter = "zzz-no-match".into();
    assert!(app.visible_ids().is_empty());
    app.extend_mark_to_cursor();
    assert!(app.select_mode.is_empty(), "nothing visible means nothing to mark");
}

#[test]
fn extending_a_range_with_a_stale_anchor_clamps() {
    // `anchor` is a raw visible-row index and rows move when the filter
    // changes; a stale anchor must clamp into the current view, not slice
    // out of bounds.
    let (_d, mut app) = three();
    app.toggle_select_mode();
    app.select_mode.anchor = Some(999);
    app.selected = 0;
    app.extend_mark_to_cursor();
    assert!(!app.select_mode.is_empty());
    assert!(app.marked_ids_in_view_order().iter().all(|id| app.tunnel(*id).is_some()));
}

#[test]
fn marking_all_in_an_empty_view_records_no_anchor() {
    let (_d, mut app) = app_with(vec![]);
    app.toggle_select_mode();
    app.mark_all_visible();
    assert!(app.select_mode.is_empty());
    assert_eq!(app.select_mode.anchor, Some(0), "an empty view's anchor saturates to 0, not underflows");
}

#[test]
fn a_batch_request_over_stale_marks_does_nothing() {
    // Marks are ids and ids can go stale (the row was deleted behind the
    // dialog); the batch must filter them out instead of acting on a ghost.
    let (_d, mut app) = three();
    app.toggle_select_mode();
    app.select_mode.mark_all(&[1, 2, 3, 999]);
    app.request_batch(BatchAction::Delete);
    // Only the three real rows show up in the prompt count.
    match &app.overlay {
        Overlay::Confirm { action: ConfirmAction::Batch { count, .. }, .. } => assert_eq!(*count, 3),
        other => panic!("expected a batch confirmation, got {other:?}"),
    }
}

#[test]
fn toggle_mark_selected_with_no_visible_rows_is_a_no_op() {
    let (_d, mut app) = three();
    app.toggle_select_mode();
    app.filter = "zzz".into();
    app.toggle_mark_selected();
    assert!(app.select_mode.is_empty());
}

#[test]
fn marked_ids_are_reported_in_display_order() {
    let (_d, mut app) = three();
    // Entering marks the cursor row (1); then mark 3 from the other end, so
    // the set was built out of order and must still be reported in order.
    app.selected = 0;
    app.toggle_select_mode();
    app.toggle_mark_selected();
    app.select_last();
    app.toggle_mark_selected();
    assert_eq!(app.select_mode.ids(), vec![1, 3], "the mark set is sorted by id");
    assert_eq!(app.marked_ids_in_view_order(), vec![1, 3], "prompts and logs should read like the table");
}
