use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use zbus::MatchRule;
use zbus::blocking::{Connection, MessageIterator, Proxy};
use zbus::zvariant::{OwnedFd, OwnedObjectPath};

use crate::{Lock, Service};

const LOGIN1_DEST: &str = "org.freedesktop.login1";
const LOGIN1_PATH: &str = "/org/freedesktop/login1";
const LOGIN1_MANAGER: &str = "org.freedesktop.login1.Manager";
const LOGIN1_SESSION: &str = "org.freedesktop.login1.Session";

const LOCK_WAIT_TIMEOUT: Duration = Duration::from_secs(2);
const LOCK_WAIT_POLL: Duration = Duration::from_millis(25);

static WATCHING: AtomicBool = AtomicBool::new(false);

/// Systemd-logind session integration for automatic locking on suspend (`PrepareForSleep`)
/// and explicit session lock requests (`loginctl lock-session`).
pub struct Session;

impl Session {
    /// Starts a background listener (once per process) that invokes `on_lock` whenever
    /// `systemd-logind` signals a suspend (`PrepareForSleep(true)`) or session `Lock`.
    ///
    /// Holds a `delay` sleep inhibitor while awake and releases it only after `Lock::read().active()`
    /// becomes true (or a 2-second safety timeout elapses) so the lock screen is rendered before
    /// the system enters sleep.
    pub fn watch_lock(on_lock: impl Fn() + Send + Sync + 'static) {
        if WATCHING.swap(true, Ordering::SeqCst) {
            return;
        }

        let on_lock = Arc::new(on_lock);
        let _ = thread::Builder::new()
            .name(String::from("amane-session"))
            .spawn(move || run_logind_loop(on_lock));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LogindEvent {
    PrepareForSleep(bool),
    SessionLock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SleepAction {
    LockAndReleaseInhibitor,
    AcquireInhibitor,
    LockOnly,
}

fn decide_action(event: LogindEvent) -> SleepAction {
    match event {
        LogindEvent::PrepareForSleep(true) => SleepAction::LockAndReleaseInhibitor,
        LogindEvent::PrepareForSleep(false) => SleepAction::AcquireInhibitor,
        LogindEvent::SessionLock => SleepAction::LockOnly,
    }
}

fn run_logind_loop(on_lock: Arc<dyn Fn() + Send + Sync + 'static>) {
    let Ok(conn) = Connection::system() else {
        return;
    };

    let Ok(manager) = Proxy::new(&conn, LOGIN1_DEST, LOGIN1_PATH, LOGIN1_MANAGER) else {
        return;
    };

    let mut inhibitor: Option<OwnedFd> = acquire_sleep_inhibitor(&manager);

    let sleep_rule = MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .interface(LOGIN1_MANAGER)
        .and_then(|b| b.member("PrepareForSleep"))
        .and_then(|b| b.path(LOGIN1_PATH))
        .map(|b| b.build());

    let Ok(sleep_rule) = sleep_rule else {
        return;
    };

    let Ok(mut sleep_stream) = MessageIterator::for_match_rule(sleep_rule, &conn, None) else {
        return;
    };

    // Also subscribe to explicit `org.freedesktop.login1.Session.Lock` on our session if available.
    if let Some(session_path) = resolve_session_path(&manager) {
        let conn_clone = conn.clone();
        let on_session_lock = Arc::clone(&on_lock);
        let _ = thread::Builder::new()
            .name(String::from("amane-session-lock"))
            .spawn(move || {
                let Ok(lock_rule) = MatchRule::builder()
                    .msg_type(zbus::message::Type::Signal)
                    .interface(LOGIN1_SESSION)
                    .and_then(|b| b.member("Lock"))
                    .and_then(|b| b.path(session_path.as_str()))
                    .map(|b| b.build())
                else {
                    return;
                };
                let Ok(stream) = MessageIterator::for_match_rule(lock_rule, &conn_clone, None)
                else {
                    return;
                };
                for msg in stream {
                    if msg.is_ok() && decide_action(LogindEvent::SessionLock) == SleepAction::LockOnly
                    {
                        on_session_lock();
                    }
                }
            });
    }

    for msg in &mut sleep_stream {
        let Ok(msg) = msg else {
            continue;
        };
        let Ok(going_to_sleep) = msg.body().deserialize::<bool>() else {
            continue;
        };

        match decide_action(LogindEvent::PrepareForSleep(going_to_sleep)) {
            SleepAction::LockAndReleaseInhibitor => {
                on_lock();
                wait_for_lock_active(LOCK_WAIT_TIMEOUT);
                inhibitor.take();
            }
            SleepAction::AcquireInhibitor => {
                if inhibitor.is_none() {
                    inhibitor = acquire_sleep_inhibitor(&manager);
                }
            }
            SleepAction::LockOnly => {
                on_lock();
            }
        }
    }
}

fn acquire_sleep_inhibitor(manager: &Proxy<'_>) -> Option<OwnedFd> {
    manager
        .call::<_, _, OwnedFd>(
            "Inhibit",
            &(
                "sleep",
                "amane",
                "Lock screen before suspend",
                "delay",
            ),
        )
        .ok()
}

fn resolve_session_path(manager: &Proxy<'_>) -> Option<OwnedObjectPath> {
    if let Ok(id) = std::env::var("XDG_SESSION_ID")
        && !id.trim().is_empty()
        && let Ok(path) = manager.call::<_, _, OwnedObjectPath>("GetSession", &(id.as_str(),))
    {
        return Some(path);
    }

    manager
        .call::<_, _, OwnedObjectPath>("GetSessionByPID", &(std::process::id(),))
        .ok()
}

fn wait_for_lock_active(timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if Lock::read().active() {
            return;
        }
        thread::sleep(LOCK_WAIT_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleep_and_lock_events_map_to_expected_actions() {
        assert_eq!(
            decide_action(LogindEvent::PrepareForSleep(true)),
            SleepAction::LockAndReleaseInhibitor
        );
        assert_eq!(
            decide_action(LogindEvent::PrepareForSleep(false)),
            SleepAction::AcquireInhibitor
        );
        assert_eq!(
            decide_action(LogindEvent::SessionLock),
            SleepAction::LockOnly
        );
    }
}
