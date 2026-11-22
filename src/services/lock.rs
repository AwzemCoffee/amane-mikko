use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use super::pam::{self, Session};
use crate::changes;
use crate::{PamConfig, PamError, PamResult, Service};

// set by Lock::start, taken by the event loop when it wakes
static REQUESTED: AtomicBool = AtomicBool::new(false);

// when true, PAM success enters `unlocking` until the lock view calls `Lock::complete_unlock()`
static DEFER_UNLOCK: AtomicBool = AtomicBool::new(false);

// the pam service file in /etc/pam.d that checks the password
const PAM_SERVICE: &str = "login";

// what the lock screen shows while App::lock holds the session
#[derive(Default)]
pub struct Lock {
    // true while the compositor holds an active ext-session-lock-v1 session
    active: bool,

    checking: bool,

    // the last authentication attempt was rejected or failed
    failed: bool,

    // pam accepted the password and the lock view is playing its exit animation
    unlocking: bool,

    // the backend ends the lock once this is set, only pam (or complete_unlock after pam) sets it
    unlocked: bool,
    attempt: Option<Arc<Session>>,
}

// changes through authentication attempts rather than polling
impl Service for Lock {
    fn new() -> Self {
        Self::default()
    }

    fn listen() {}
}

impl Lock {
    // true while the session is locked by the compositor
    pub fn active(&self) -> bool {
        self.active
    }

    // an authentication attempt is in progress
    pub fn checking(&self) -> bool {
        self.checking
    }

    pub fn failed(&self) -> bool {
        self.failed
    }

    /// True after PAM authentication succeeded while waiting for `Lock::complete_unlock()`.
    pub fn unlocking(&self) -> bool {
        self.unlocking
    }

    pub(crate) fn unlocked(&self) -> bool {
        self.unlocked
    }

    /// Configures whether PAM success should wait for `Lock::complete_unlock()` before releasing
    /// the compositor session lock (allowing the lock view to play an exit animation).
    pub fn set_deferred_unlock(defer: bool) {
        DEFER_UNLOCK.store(defer, Ordering::Relaxed);
    }

    /// Releases the compositor session lock once a deferred unlock animation has finished.
    /// Has no effect unless PAM authentication has already succeeded (`Lock::unlocking()`).
    pub fn complete_unlock() {
        let mut lock = Self::write();
        if !lock.unlocking || lock.unlocked {
            lock.quiet();
            return;
        }
        lock.unlocked = true;
    }

    pub(crate) fn set_active(active: bool) {
        let mut lock = Self::write();
        if lock.active == active {
            lock.quiet();
            return;
        }
        lock.active = active;
    }

    /*
     * locks the session with the view given to App::lock, from a button,
     * an ipc call or a thread; does nothing while already locked
     */
    pub fn start() {
        REQUESTED.store(true, Ordering::Relaxed);

        changes::mark_all();
    }

    pub(crate) fn take_request() -> bool {
        REQUESTED.swap(false, Ordering::Relaxed)
    }

    // a new lock starts clean, not with the last one's unlocked or failed state
    pub(crate) fn reset() {
        let attempt = {
            let mut lock = Self::write();
            let attempt = lock.attempt.take();
            if let Some(attempt) = &attempt {
                attempt.abort();
            }
            *lock = Self::default();
            attempt
        };
        if let Some(attempt) = attempt {
            pam::abort_session(&attempt);
        }
    }

    /*
     * pam can take seconds to answer, so it runs on its own thread
     * and the screen unlocks only if it accepts the password
     */
    pub fn unlock(password: &str) {
        Self::unlock_with(PAM_SERVICE, password);
    }

    /// Authenticates using a specific PAM service name in `/etc/pam.d/`.
    /// Falls back to `"login"` when `service` is empty or whitespace.
    pub fn unlock_with(service: &str, password: &str) {
        let service = normalize_pam_service(service);
        let _ = Self::begin(PamConfig::new(service), Some(String::from(password)));
    }

    /// Starts interactive unlocking for the current user. Use `Pam` to read and answer prompts.
    pub fn authenticate(config: PamConfig) -> Result<(), PamError> {
        Self::begin(config, None)
    }

    /// Cancels this lock's authentication attempt and keeps the session locked.
    pub fn abort() {
        let attempt = {
            let mut lock = Self::write();
            let attempt = lock.attempt.take();
            if let Some(attempt) = &attempt {
                attempt.abort();
            }
            lock.checking = false;
            lock.failed = false;
            lock.unlocking = false;
            lock.unlocked = false;
            attempt
        };
        if let Some(attempt) = attempt {
            pam::abort_session(&attempt);
        }
    }

    fn begin(config: PamConfig, password: Option<String>) -> Result<(), PamError> {
        let config = config.for_lock()?;
        let attempt = Arc::new(Session::default());
        {
            let mut lock = Self::write();
            if lock.checking || lock.unlocking {
                lock.quiet();
                return Err(PamError::Busy);
            }
            lock.checking = true;
            lock.failed = false;
            lock.unlocking = false;
            lock.unlocked = false;
            lock.attempt = Some(attempt.clone());
        }
        // Views can read both services in either order, so their write guards must never overlap.
        let result = pam::start(config, password, attempt.clone(), Some(Self::finished));
        if result.is_err() {
            let mut lock = Self::write();
            if attempt.is_current(&lock.attempt) {
                lock.attempt = None;
                lock.checking = false;
                lock.failed = true;
            } else {
                lock.quiet();
            }
        }
        result
    }

    fn finished(attempt: &Arc<Session>, result: &PamResult) {
        let mut lock = Self::write();
        if !attempt.is_current(&lock.attempt) {
            lock.quiet();
            return;
        }
        lock.attempt = None;
        lock.checking = false;
        lock.failed = matches!(result, PamResult::Rejected(_) | PamResult::Error(_));
        let succeeded = !attempt.aborted() && *result == PamResult::Success;
        if succeeded && DEFER_UNLOCK.load(Ordering::Relaxed) {
            lock.unlocking = true;
            lock.unlocked = false;
        } else {
            lock.unlocking = false;
            lock.unlocked = succeeded;
        }
    }
}

fn normalize_pam_service(service: &str) -> &str {
    let trimmed = service.trim();
    if trimmed.is_empty() {
        PAM_SERVICE
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_pam_service_defaults_blank_to_login() {
        assert_eq!(normalize_pam_service(""), "login");
        assert_eq!(normalize_pam_service("   "), "login");
        assert_eq!(normalize_pam_service("  kultsi-lock "), "kultsi-lock");
    }

    #[test]
    fn lock_active_transitions_cleanly() {
        Lock::set_active(false);
        assert!(!Lock::read().active());
        Lock::set_active(true);
        assert!(Lock::read().active());
        Lock::set_active(false);
        assert!(!Lock::read().active());
    }

    #[test]
    fn deferred_unlock_waits_for_complete_unlock_and_blocks_bypass() {
        Lock::reset();
        Lock::set_deferred_unlock(true);

        // Calling complete_unlock before PAM succeeds must never unlock.
        Lock::complete_unlock();
        assert!(!Lock::read().unlocking());
        assert!(!Lock::read().unlocked());

        // Simulate PAM success on the active attempt.
        let attempt = Arc::new(Session::default());
        {
            let mut lock = Lock::write();
            lock.checking = true;
            lock.attempt = Some(attempt.clone());
        }
        Lock::finished(&attempt, &PamResult::Success);

        assert!(Lock::read().unlocking());
        assert!(!Lock::read().unlocked());

        // Finishing the exit animation releases the session lock.
        Lock::complete_unlock();
        assert!(Lock::read().unlocked());

        Lock::set_deferred_unlock(false);
        Lock::reset();
    }
}
