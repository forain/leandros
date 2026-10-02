//! org.freedesktop.RealtimeKit1: a RealtimeKit that grants nothing.
//!
//! xdg-desktop-portal (ports/portal) reads RealtimeKit's limits when it
//! starts, for its Realtime portal; with no service it logged three
//! ServiceUnknown warnings per start. LeandrOS has no SCHED_RR/SCHED_FIFO or
//! nice levels a session user may raise, so the limits are reported as
//! zero (no realtime priority, no nice boost, no RT time) and every request
//! is refused with NotSupported rather than claimed to succeed.

const NAME: &str = "org.freedesktop.RealtimeKit1";
const PATH: &str = "/org/freedesktop/RealtimeKit1";

fn refuse(what: &str) -> zbus::fdo::Error {
    crate::log("rtkit", format_args!("refused {what}: no realtime scheduling on LeandrOS"));
    zbus::fdo::Error::NotSupported("LeandrOS has no realtime or nice-level scheduling for session processes".into())
}

struct RealtimeKit;

#[zbus::interface(name = "org.freedesktop.RealtimeKit1")]
impl RealtimeKit {
    fn make_thread_realtime(&self, _thread: u64, _priority: u32) -> zbus::fdo::Result<()> {
        Err(refuse("MakeThreadRealtime"))
    }
    #[zbus(name = "MakeThreadRealtimeWithPID")]
    fn make_thread_realtime_with_pid(&self, _pid: u64, _thread: u64, _priority: u32) -> zbus::fdo::Result<()> {
        Err(refuse("MakeThreadRealtimeWithPID"))
    }
    fn make_thread_high_priority(&self, _thread: u64, _nice: i32) -> zbus::fdo::Result<()> {
        Err(refuse("MakeThreadHighPriority"))
    }
    #[zbus(name = "MakeThreadHighPriorityWithPID")]
    fn make_thread_high_priority_with_pid(&self, _pid: u64, _thread: u64, _nice: i32) -> zbus::fdo::Result<()> {
        Err(refuse("MakeThreadHighPriorityWithPID"))
    }
    #[zbus(property)]
    fn max_realtime_priority(&self) -> i32 {
        0
    }
    #[zbus(property)]
    fn min_nice_level(&self) -> i32 {
        0
    }
    #[zbus(property, name = "RTTimeUSecMax")]
    fn rt_time_usec_max(&self) -> i64 {
        0
    }
}

pub async fn serve(builder: zbus::connection::Builder<'_>) -> zbus::Result<zbus::Connection> {
    builder.serve_at(PATH, RealtimeKit)?.name(NAME)?.build().await
}
