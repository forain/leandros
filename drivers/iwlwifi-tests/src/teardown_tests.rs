//! Soft-teardown command order and the session-protection time event
//! (openbugs L12 #66: TIME_EVENT_CMD REMOVE of an event that had already
//! ended crashed firmware -36 with NMI_INTERRUPT_DATA_ACTION_PT).
use crate::iwl_assoc::TeardownStep::*;
use crate::iwl_assoc::*;

const START: u32 = 1;
const END: u32 = 2;

#[test]
fn teardown_order_matches_iwlmvm() {
    // Flush + drain, queues (BA) off, MAC unassociated, AP station removed
    // (firmware refuses it while associated), time event, then unbind.
    assert_eq!(teardown_plan(true, true, Some(7)),
               vec![FlushTx, DisableQueues, MacUnassoc, RemoveSta, RemoveTimeEvent(7), Unbind]);
    assert_eq!(teardown_plan(true, false, None), vec![MacUnassoc, Unbind]);
    assert_eq!(teardown_plan(false, false, None), vec![]);
}

#[test]
fn ended_time_event_is_not_removed() {
    let mut t = TeTrack::new();
    t.on_add(0x40);
    t.on_notif(0x40, START, 1);
    assert_eq!(t.remove_uid(), Some(0x40));
    t.on_notif(0x40, END, 1);
    assert_eq!(t.remove_uid(), None);
    assert!(!teardown_plan(true, true, t.remove_uid()).iter().any(|s| matches!(s, RemoveTimeEvent(_))));
}

#[test]
fn time_event_that_failed_to_start_is_not_removed() {
    let mut t = TeTrack::new();
    t.on_add(9);
    t.on_notif(9, START, 0);
    assert_eq!(t.remove_uid(), None);
}

#[test]
fn end_seen_before_the_add_response_is_kept() {
    // The notification can be dispatched while the ADD response is awaited.
    let mut t = TeTrack::new();
    t.on_notif(5, END, 1);
    t.on_add(5);
    assert_eq!(t.remove_uid(), None);
}

#[test]
fn stale_notification_does_not_hide_the_running_event() {
    let mut t = TeTrack::new();
    t.on_add(5);
    t.on_notif(5, END, 1);
    t.on_add(6);
    t.on_notif(5, END, 1);
    assert_eq!(t.remove_uid(), Some(6));
    t.removed();
    assert_eq!(t.remove_uid(), None);
}
