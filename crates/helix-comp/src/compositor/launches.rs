//! Launch feedback (helixdesktop/spec `contracts/wayland/README.md`,
//! "Launch feedback"; helixdesktop/desktop#291): while an app the shell
//! launched is starting, the pointer shows the "progress" cursor (an arrow
//! with a spinner: everything still works) where it would show the plain
//! arrow over the desktop or the shell. Over app windows (their title bars
//! too) the app's own cursor wins, as mutter's busy cursor only overrides
//! its own stage's.
//!
//! A launch starts when the shell says so: `gtk_shell1.notify_launch`
//! with the activation token it hands the app (`DESKTOP_STARTUP_ID`,
//! `XDG_ACTIVATION_TOKEN`, `desktop-startup-id`). Tokens made for anything
//! else (passing focus, a notification's action) are not launches. An X11
//! launcher's `_NET_STARTUP_INFO` "new:" starts one as well, as in mutter.
//!
//! A launch ends at the first of:
//!
//! - its id is used: a window activated with the token (`xdg_activation_v1`,
//!   `gtk_surface1.request_focus`, an X11 window's `_NET_STARTUP_ID` with
//!   `_NET_ACTIVE_WINDOW`), which is also how a single-instance app hands a
//!   launch to its running copy; or `gtk_shell1.set_startup_id`;
//! - a new X11 window maps with it as `_NET_STARTUP_ID` (its own or its
//!   client leader's);
//! - a new window maps whose app is the launched one: its Wayland app id,
//!   or X11 `WM_CLASS` class or instance, is the launch's app id (the
//!   desktop file id the shell gave the token, without `.desktop`) or an
//!   X11 launcher's `WMCLASS`, ignoring case (for apps that never use the
//!   token; mutter matches X11 `WMCLASS` the same way);
//! - `_NET_STARTUP_INFO` "remove:" with its `ID`;
//! - 15 s pass (mutter's `STARTUP_TIMEOUT_MS`; `HELIX_LAUNCH_TIMEOUT_MS`
//!   changes it, for tests).
//!
//! Launches overlap: the pointer is busy while any is pending.
//!
//! When a launch the shell announced ends, the shell is told
//! (`helix_window_list_v1.launch_ended`, version 14, with why), so its dock
//! stops showing the app as starting (helixdesktop/desktop#298). Only the
//! client that announced a launch hears of its end.

use std::time::{Duration, Instant};

use smithay::input::pointer::{CursorIcon, CursorImageStatus};
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::Resource;

use super::window::Window;
use super::Helix;

/// How long a launch shows at most (mutter's `STARTUP_TIMEOUT_MS`).
pub const TIMEOUT: Duration = Duration::from_secs(15);

/// At most this many launches are followed (the oldest goes).
const MAX_PENDING: usize = 32;

/// Launch ids longer than this are ignored.
const MAX_ID: usize = 1024;

#[derive(Debug)]
pub struct Launches {
    pending: Vec<Launch>,
    /// X11 windows whose startup id was asked for (once each).
    asked: Vec<u32>,
    timeout: Duration,
}

impl Default for Launches {
    fn default() -> Self {
        let timeout = std::env::var("HELIX_LAUNCH_TIMEOUT_MS").ok().and_then(|v| v.parse().ok()).map(Duration::from_millis).unwrap_or(TIMEOUT);
        Self { pending: Vec::new(), asked: Vec::new(), timeout }
    }
}

/// Why a launch ended (`helix_window_list_v1.launch_end`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchEnd {
    /// Its id was used: a window activated with it, or an app set it.
    Used,
    /// A new window of its app mapped (by app id, `WM_CLASS` or
    /// `_NET_STARTUP_ID`).
    Window,
    /// `_NET_STARTUP_INFO` "remove:".
    Removed,
    TimedOut,
    /// Too many were pending: the oldest went.
    Dropped,
}

impl LaunchEnd {
    fn why(self) -> &'static str {
        match self {
            Self::Used => "its id was used",
            Self::Window => "its app mapped a window",
            Self::Removed => "the app said so (_NET_STARTUP_INFO remove)",
            Self::TimedOut => "timed out",
            Self::Dropped => "too many launches pending",
        }
    }

    fn wire(self) -> helix_protocols::server::helix_window_list_v1::LaunchEnd {
        use helix_protocols::server::helix_window_list_v1::LaunchEnd as W;
        match self {
            Self::Used => W::Used,
            Self::Window => W::Window,
            Self::Removed => W::Removed,
            Self::TimedOut => W::TimedOut,
            Self::Dropped => W::Dropped,
        }
    }
}

#[derive(Debug)]
struct Launch {
    id: String,
    /// The client that announced it (`gtk_shell1.notify_launch`), told
    /// when it ends; `None` for an X11 launcher's.
    by: Option<ClientId>,
    /// What a new window's app id or `WM_CLASS` is compared with
    /// (lowercase).
    app: Option<String>,
    deadline: Instant,
    /// The windows there were when it started.
    known: Vec<Window>,
}

/// The app a desktop file id, app id or `WM_CLASS` names, for comparing.
fn app_key(id: &str) -> Option<String> {
    let id = id.strip_suffix(".desktop").unwrap_or(id).trim();
    (!id.is_empty()).then(|| id.to_lowercase())
}

impl Launches {
    /// Whether any launch is pending.
    pub fn busy(&self) -> bool {
        !self.pending.is_empty()
    }
}

impl Helix {
    /// Every window, shown or not.
    fn all_windows(&self) -> Vec<Window> {
        self.space.elements().chain(self.minimized_windows()).chain(self.workspaces.parked()).cloned().collect()
    }

    /// A launch with startup id `id` started, of app `app` (a desktop file
    /// id or `WM_CLASS`, if known), announced by client `by` (told when it
    /// ends).
    pub fn launch_started(&mut self, id: &str, app: Option<&str>, by: Option<ClientId>) {
        if id.is_empty() || id.len() > MAX_ID || self.launches.pending.iter().any(|l| l.id == id) {
            return;
        }
        if self.launches.pending.len() >= MAX_PENDING {
            let oldest = self.launches.pending[0].id.clone();
            self.launch_ended(&oldest, LaunchEnd::Dropped);
        }
        let app = app.and_then(app_key);
        tracing::debug!(app, pending = self.launches.pending.len() + 1, "launch started");
        let known = self.all_windows();
        let deadline = Instant::now() + self.launches.timeout;
        self.launches.pending.push(Launch { id: id.to_owned(), by, app, deadline, known });
    }

    /// The launch with startup id `id` (if any) is over, for `why`: its
    /// announcer's window lists hear of it.
    pub fn launch_ended(&mut self, id: &str, why: LaunchEnd) {
        if let Some(i) = self.launches.pending.iter().position(|l| l.id == id) {
            let launch = self.launches.pending.remove(i);
            tracing::debug!(why = why.why(), told = launch.by.is_some(), pending = self.launches.pending.len(), "launch ended");
            if let Some(by) = launch.by {
                for listener in self.window_list.listeners.iter().filter(|l| l.version() >= 14 && Resource::client(*l).is_some_and(|c| c.id() == by)) {
                    listener.launch_ended(launch.id.clone(), why.wire());
                }
            }
        }
        if self.launches.pending.is_empty() {
            self.launches.asked.clear();
        }
    }

    /// Time launches out, and end those whose app mapped a window (by app
    /// id or `WM_CLASS`); ask new X11 windows for their startup id.
    pub fn launches_tick(&mut self, now: Instant) {
        if self.launches.pending.is_empty() {
            return;
        }
        let expired: Vec<String> = self.launches.pending.iter().filter(|l| now >= l.deadline).map(|l| l.id.clone()).collect();
        for id in expired {
            self.launch_ended(&id, LaunchEnd::TimedOut);
        }
        let windows = self.all_windows();
        let mut ended = Vec::new();
        for window in &windows {
            let new = self.launches.pending.iter().any(|l| !l.known.contains(window));
            if !new {
                continue;
            }
            let mut apps = Vec::new();
            if let Some(x11) = window.x11_surface() {
                apps.extend([x11.class(), x11.instance()]);
                let xid = x11.window_id();
                if !self.launches.asked.contains(&xid) {
                    if let Some(ping) = self.x11ping.as_ref() {
                        self.launches.asked.push(xid);
                        ping.ask_new_window_startup_id(xid);
                    }
                }
            } else {
                apps.push(super::window_list::describe(window).0);
            }
            let apps: Vec<String> = apps.iter().filter_map(|a| app_key(a)).collect();
            for launch in self.launches.pending.iter().filter(|l| !l.known.contains(window)) {
                if launch.app.as_ref().is_some_and(|a| apps.contains(a)) {
                    ended.push(launch.id.clone());
                }
            }
        }
        for id in ended {
            self.launch_ended(&id, LaunchEnd::Window);
        }
    }

    /// The new X11 window `window`'s startup id (`_NET_STARTUP_ID`, its
    /// own or its client leader's) was read.
    pub(super) fn x11_new_window_startup_id(&mut self, _window: u32, startup_id: Option<String>) {
        if let Some(id) = startup_id {
            self.launch_ended(&id, LaunchEnd::Window);
        }
    }

    /// An X11 startup notification message (`_NET_STARTUP_INFO`).
    pub(super) fn x11_startup_info(&mut self, info: super::x11ping::StartupInfo) {
        use super::x11ping::StartupKind;
        let Some(id) = info.get("ID") else { return };
        let id = id.to_owned();
        match info.kind {
            StartupKind::New => {
                let app = info.get("WMCLASS").map(str::to_owned);
                self.launch_started(&id, app.as_deref(), None);
            }
            StartupKind::Remove => self.launch_ended(&id, LaunchEnd::Removed),
            StartupKind::Change => {}
        }
    }

    /// Whether the pointer shows the launch's busy cursor where it is now
    /// (any launch pending): over the desktop or the shell, where it would
    /// show the plain arrow; not over app windows.
    fn launch_busy_here(&self) -> bool {
        if !self.launches.busy() || self.lock.locked || self.cursor_status != CursorImageStatus::Named(CursorIcon::Default) {
            return false;
        }
        let Some(pointer) = self.seat.get_pointer() else { return false };
        match pointer.current_focus() {
            // The shell's surfaces.
            Some(focus) => focus.surface().and_then(|s| s.client()).is_some_and(|c| super::privileged(&c)),
            // The desktop, or a window's title bar (part of the window).
            None => {
                let pos = pointer.current_location();
                self.window_under(pos).is_none() && self.frame_under(pos).is_none()
            }
        }
    }

    /// The pointer image to show: [`Helix::cursor_status`], or the busy
    /// cursor while a launch is pending ([`Self::launch_busy_here`]).
    pub fn shown_cursor(&self) -> CursorImageStatus {
        if self.launch_busy_here() {
            CursorImageStatus::Named(CursorIcon::Progress)
        } else {
            self.cursor_status.clone()
        }
    }

    /// [`Self::shown_cursor`] as a name (tests): the cursor's, `surface`
    /// or `hidden`.
    pub fn shown_cursor_name(&self) -> String {
        match self.shown_cursor() {
            CursorImageStatus::Named(icon) => icon.name().to_owned(),
            CursorImageStatus::Surface(_) => "surface".into(),
            CursorImageStatus::Hidden => "hidden".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::app_key;

    #[test]
    fn app_keys() {
        assert_eq!(app_key("org.gnome.Calculator.desktop").as_deref(), Some("org.gnome.calculator"));
        assert_eq!(app_key("org.gnome.Calculator").as_deref(), Some("org.gnome.calculator"));
        assert_eq!(app_key("Firefox").as_deref(), Some("firefox"));
        assert_eq!(app_key(""), None);
        assert_eq!(app_key(".desktop"), None);
    }
}
