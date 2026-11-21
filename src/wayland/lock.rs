use smithay_client_toolkit::session_lock::{
    SessionLock, SessionLockHandler, SessionLockSurface, SessionLockSurfaceConfigure,
};
use wayland_client::{Connection, QueueHandle, protocol::wl_output::WlOutput};

use crate::{Lock, Service};

use super::{
    WaylandState, output,
    surface::{Role, View},
};

trait LockDriver {
    type Output: PartialEq + Clone;

    fn has_view(&self) -> bool;
    fn has_session_lock(&self) -> bool;
    fn request_lock(&mut self) -> bool;
    fn outputs(&self) -> Vec<Self::Output>;
    fn has_lock_surface(&self, output: &Self::Output) -> bool;
    fn create_lock_surface(&mut self, output: Self::Output);
    fn set_lock_active(&mut self, active: bool);
    fn clear_lock(&mut self);
}

fn open_lock_for<D: LockDriver>(driver: &mut D, output: D::Output) {
    if !driver.has_view() || !driver.has_session_lock() || driver.has_lock_surface(&output) {
        return;
    }

    driver.create_lock_surface(output);
}

fn open_all_lock_surfaces<D: LockDriver>(driver: &mut D) {
    for output in driver.outputs() {
        open_lock_for(driver, output);
    }
}

// Create lock surfaces immediately after lock(): compositors may wait for initial lock surface
// commits before emitting locked() (otherwise stalling until a multi-second fallback timeout).
fn start_lock_flow<D: LockDriver>(driver: &mut D) {
    if driver.has_session_lock() {
        return;
    }

    if driver.request_lock() {
        open_all_lock_surfaces(driver);
    }
}

fn on_locked_flow<D: LockDriver>(driver: &mut D) {
    driver.set_lock_active(true);
    open_all_lock_surfaces(driver);
}

fn on_finished_flow<D: LockDriver>(driver: &mut D) {
    driver.clear_lock();
    driver.set_lock_active(false);
}

impl LockDriver for WaylandState {
    type Output = WlOutput;

    fn has_view(&self) -> bool {
        self.lock_view.is_some()
    }

    fn has_session_lock(&self) -> bool {
        self.session_lock.is_some()
    }

    fn request_lock(&mut self) -> bool {
        let Some(lock_state) = &self.lock_state else {
            eprintln!("amane: Lock::start needs a lock screen, see App::lock");
            return false;
        };

        Lock::reset();

        // a compositor without ext-session-lock cannot lock, which must not take the shell down
        let Ok(session_lock) = lock_state.lock(&self.qh) else {
            eprintln!("amane: the compositor does not support ext-session-lock");
            return false;
        };

        self.session_lock = Some(session_lock);
        true
    }

    fn outputs(&self) -> Vec<WlOutput> {
        self.output.outputs().collect()
    }

    fn has_lock_surface(&self, output: &WlOutput) -> bool {
        self.windows.iter().any(|window| {
            matches!(window.role, Role::Lock(_)) && window.output.as_ref() == Some(output)
        })
    }

    fn create_lock_surface(&mut self, output: WlOutput) {
        let (Some(view), Some(session_lock)) = (self.lock_view, &self.session_lock) else {
            return;
        };

        let Some(info) = self.output.info(&output) else {
            return;
        };

        let view = View::Monitor(view, output::describe(&info, self.scale_factor));
        let surface = self.compositor.create_surface(&self.qh);
        let lock_surface = session_lock.create_lock_surface(surface, &output, &self.qh);

        self.add(view, Some(output), Role::Lock(lock_surface));
    }

    fn set_lock_active(&mut self, active: bool) {
        Lock::set_active(active);
    }

    fn clear_lock(&mut self) {
        self.session_lock = None;
        self.close_lock_screens();
    }
}

impl WaylandState {
    // every monitor gets its own lock screen, the compositor shows nothing else while locked
    pub fn open_lock(&mut self, output: WlOutput) {
        open_lock_for(self, output);
    }

    // runs on every wake, since that is when a Lock::start shows up
    pub fn start_lock_if_asked(&mut self) {
        if !Lock::take_request() {
            return;
        }

        start_lock_flow(self);
    }

    // runs on every wake, since that is when a finished pam check shows up
    pub fn end_lock_if_unlocked(&mut self) {
        let Some(session_lock) = &self.session_lock else {
            return;
        };

        if !Lock::read().unlocked() {
            return;
        }

        session_lock.unlock();

        self.clear_lock();
        Lock::set_active(false);

        /*
         * the unlock has to reach the compositor before amane exits,
         * or the session stays locked with nobody left to unlock it
         */
        self.connection
            .flush()
            .expect("failed to send unlock to the compositor");
    }

    fn close_lock_screens(&mut self) {
        self.windows
            .retain(|window| !matches!(window.role, Role::Lock(_)));

        if self.windows.is_empty() && self.per_monitor.is_empty() {
            self.running = false;
        }
    }
}

impl SessionLockHandler for WaylandState {
    fn locked(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        on_locked_flow(self);
    }

    // the compositor refused, most likely because another locker already holds the session
    fn finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: SessionLock) {
        eprintln!("amane: the compositor refused or ended the session lock");

        on_finished_flow(self);
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        lock_surface: SessionLockSurface,
        configure: SessionLockSurfaceConfigure,
        _: u32,
    ) {
        let Some(window) = self.window(lock_surface.wl_surface()) else {
            return;
        };

        // a lock screen always covers its whole monitor
        let (width, height) = configure.new_size;

        window.resize(width, height);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct FakeLockDriver {
        has_view: bool,
        allow_lock: bool,
        session_lock: bool,
        active: bool,
        outputs: Vec<&'static str>,
        surfaces: Vec<&'static str>,
        lock_requests: usize,
    }

    impl LockDriver for FakeLockDriver {
        type Output = &'static str;

        fn has_view(&self) -> bool {
            self.has_view
        }

        fn has_session_lock(&self) -> bool {
            self.session_lock
        }

        fn request_lock(&mut self) -> bool {
            self.lock_requests += 1;
            if self.allow_lock {
                self.session_lock = true;
                true
            } else {
                false
            }
        }

        fn outputs(&self) -> Vec<&'static str> {
            self.outputs.clone()
        }

        fn has_lock_surface(&self, output: &&'static str) -> bool {
            self.surfaces.contains(output)
        }

        fn create_lock_surface(&mut self, output: &'static str) {
            self.surfaces.push(output);
        }

        fn set_lock_active(&mut self, active: bool) {
            self.active = active;
        }

        fn clear_lock(&mut self) {
            self.session_lock = false;
            self.surfaces.clear();
        }
    }

    #[test]
    fn creates_lock_surfaces_immediately_before_compositor_sends_locked() {
        let mut driver = FakeLockDriver {
            has_view: true,
            allow_lock: true,
            outputs: vec!["DP-1", "HDMI-A-1"],
            ..Default::default()
        };

        start_lock_flow(&mut driver);

        // Lock surfaces must exist BEFORE locked() arrives so compositors do not stall on timeout.
        assert!(driver.session_lock);
        assert!(!driver.active);
        assert_eq!(driver.surfaces, vec!["DP-1", "HDMI-A-1"]);

        // Compositor emits locked(): marks active without duplicating surfaces.
        on_locked_flow(&mut driver);
        assert!(driver.active);
        assert_eq!(driver.surfaces, vec!["DP-1", "HDMI-A-1"]);
    }

    #[test]
    fn new_output_and_finished_manage_surfaces_cleanly() {
        let mut driver = FakeLockDriver {
            has_view: true,
            allow_lock: true,
            outputs: vec!["DP-1"],
            ..Default::default()
        };

        start_lock_flow(&mut driver);
        driver.outputs.push("DP-2");
        open_lock_for(&mut driver, "DP-2");
        assert_eq!(driver.surfaces, vec!["DP-1", "DP-2"]);

        // Duplicate start_lock_flow while locked is ignored.
        start_lock_flow(&mut driver);
        assert_eq!(driver.lock_requests, 1);

        // Refusal or compositor finish tears down surfaces and clears active state.
        on_locked_flow(&mut driver);
        on_finished_flow(&mut driver);
        assert!(!driver.session_lock);
        assert!(!driver.active);
        assert!(driver.surfaces.is_empty());
    }
}
