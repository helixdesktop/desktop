//! Apps the shell launched that are still starting (helixdesktop/spec
//! `contracts/wayland/README.md`, "Launch feedback (Helix)";
//! helixdesktop/desktop#298): the dock (and the overview's dash) show them
//! dimmed, breathing gently with animations, until the launch ends.
//!
//! A launch starts when the shell announces its token
//! (`activation::launch_token`, `gtk_shell1.notify_launch`), on whatever
//! thread launched it; it ends when helix-comp says so
//! (`helix_window_list_v1.launch_ended`: its token was used, a window of
//! the app mapped, the X11 app removed it, or helix-comp's timeout), or,
//! should that never come (an older compositor, a restarted one), after a
//! safety timeout a little over helix-comp's.

use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// helix-comp's launch timeout (mutter's `STARTUP_TIMEOUT_MS`; it reads
/// `HELIX_LAUNCH_TIMEOUT_MS` too).
const COMP_TIMEOUT: Duration = Duration::from_secs(15);
/// How much longer the shell waits for helix-comp's end.
const MARGIN: Duration = Duration::from_secs(1);

/// How long the dimmed icon takes to breathe in and out.
pub const PULSE_PERIOD: Duration = Duration::from_millis(1600);
/// The starting icon's opacity: still (no animations, or reduced motion),
/// and the range it breathes through.
pub const STILL_ALPHA: f32 = 0.5;
pub const PULSE_MIN: f32 = 0.45;
pub const PULSE_MAX: f32 = 0.85;
/// How often a breathing icon is redrawn.
pub const PULSE_FRAME: Duration = Duration::from_millis(50);

/// The safety timeout: helix-comp's (`HELIX_LAUNCH_TIMEOUT_MS`) and a
/// margin; `HELIX_SHELL_LAUNCH_TIMEOUT_MS` sets it outright (tests).
pub fn safety_timeout() -> Duration {
    let ms = |var: &str| std::env::var(var).ok().and_then(|v| v.parse::<u64>().ok()).map(Duration::from_millis);
    ms("HELIX_SHELL_LAUNCH_TIMEOUT_MS").unwrap_or_else(|| ms("HELIX_LAUNCH_TIMEOUT_MS").unwrap_or(COMP_TIMEOUT) + MARGIN)
}

/// The app a desktop file id names (without `.desktop`).
fn app_of(desktop_id: &str) -> &str {
    desktop_id.strip_suffix(".desktop").unwrap_or(desktop_id)
}

#[derive(Debug, Clone)]
struct Launch {
    token: String,
    /// The app (desktop file id without `.desktop`).
    app: String,
    since: Instant,
    deadline: Instant,
}

/// The launches still starting.
#[derive(Debug, Clone)]
pub struct Starting {
    launches: Vec<Launch>,
    timeout: Duration,
}

impl Starting {
    /// Nothing starting; launches give up after `timeout`.
    pub fn new(timeout: Duration) -> Self {
        Self { launches: Vec::new(), timeout }
    }

    /// The launch with token `token` of app `desktop_id` started at `now`.
    pub fn started(&mut self, token: &str, desktop_id: &str, now: Instant) {
        let app = app_of(desktop_id).trim();
        if token.is_empty() || app.is_empty() || self.launches.iter().any(|l| l.token == token) {
            return;
        }
        // As helix-comp's, at most 32 are followed.
        if self.launches.len() >= 32 {
            self.launches.remove(0);
        }
        self.launches.push(Launch { token: token.to_owned(), app: app.to_owned(), since: now, deadline: now + self.timeout });
    }

    /// The launch with token `token` ended: its app, if it was one.
    pub fn ended(&mut self, token: &str) -> Option<String> {
        let i = self.launches.iter().position(|l| l.token == token)?;
        Some(self.launches.remove(i).app)
    }

    /// Give up on launches past the safety timeout at `now`: their apps.
    pub fn expire(&mut self, now: Instant) -> Vec<String> {
        let (gone, kept): (Vec<Launch>, Vec<Launch>) = self.launches.drain(..).partition(|l| now >= l.deadline);
        self.launches = kept;
        gone.into_iter().map(|l| l.app).collect()
    }

    /// When the next launch gives up.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.launches.iter().map(|l| l.deadline).min()
    }

    /// The apps starting, each once, oldest launch first.
    pub fn apps(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for l in &self.launches {
            if !out.iter().any(|a| a.eq_ignore_ascii_case(&l.app)) {
                out.push(l.app.clone());
            }
        }
        out
    }

    /// Since when `app` (an app id, with or without `.desktop`) is
    /// starting (its newest launch), if it is.
    pub fn since(&self, app: &str) -> Option<Instant> {
        let app = app_of(app);
        self.launches.iter().filter(|l| l.app.eq_ignore_ascii_case(app)).map(|l| l.since).max()
    }

    pub fn is_empty(&self) -> bool {
        self.launches.is_empty()
    }
}

/// A starting app's icon opacity `elapsed` after its launch: breathing
/// between [`PULSE_MIN`] and [`PULSE_MAX`] when `pulse`, else
/// [`STILL_ALPHA`].
pub fn icon_alpha(elapsed: Duration, pulse: bool) -> f32 {
    if !pulse {
        return STILL_ALPHA;
    }
    let phase = elapsed.as_secs_f64() / PULSE_PERIOD.as_secs_f64() * std::f64::consts::TAU;
    // Starts bright (as the icon was) and dims, smoothly.
    let t = (1.0 + phase.cos()) / 2.0;
    PULSE_MIN + (PULSE_MAX - PULSE_MIN) * t as f32
}

/// Whether starting icons breathe with `motion`: only with full motion
/// (no animations or reduced motion: still and dimmed).
pub fn pulses(motion: helix_draw::motion::Motion) -> bool {
    motion == helix_draw::motion::Motion::Full
}

/// The shell's launches (any thread starts them; the UI ends them).
static STATE: OnceLock<Mutex<Starting>> = OnceLock::new();
/// Wakes the UI when a launch starts.
static WAKE: OnceLock<Box<dyn Fn() + Send + Sync>> = OnceLock::new();

fn state() -> std::sync::MutexGuard<'static, Starting> {
    STATE.get_or_init(|| Mutex::new(Starting::new(safety_timeout()))).lock().unwrap_or_else(|e| e.into_inner())
}

/// Call `wake` (on any thread) whenever a launch starts (once).
pub fn on_start(wake: impl Fn() + Send + Sync + 'static) {
    let _ = WAKE.set(Box::new(wake));
}

/// The shell announced the launch with token `token` of `desktop_id`.
pub fn started(token: &str, desktop_id: &str) {
    state().started(token, desktop_id, Instant::now());
    tracing::info!(app = app_of(desktop_id), "app starting");
    if let Some(wake) = WAKE.get() {
        wake();
    }
}

/// helix-comp says the launch with token `token` ended (`reason`): its app.
pub fn ended(token: &str, reason: &str) -> Option<String> {
    let app = state().ended(token);
    if let Some(app) = &app {
        tracing::info!(app, reason, "app no longer starting");
    }
    app
}

/// Give up on launches past the safety timeout: their apps.
pub fn expire() -> Vec<String> {
    let gone = state().expire(Instant::now());
    for app in &gone {
        tracing::info!(app, reason = "the shell's safety timeout", "app no longer starting");
    }
    gone
}

/// What is starting now.
pub fn snapshot() -> Starting {
    state().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launches_start_end_and_expire() {
        let t0 = Instant::now();
        let mut s = Starting::new(Duration::from_secs(16));
        s.started("t1", "org.example.Calc.desktop", t0);
        s.started("t1", "org.example.Calc.desktop", t0);
        s.started("t2", "org.example.Edit", t0 + Duration::from_secs(1));
        s.started("t3", "org.example.calc.desktop", t0 + Duration::from_secs(2));
        // No app, no token: not followed.
        s.started("t4", "", t0);
        s.started("", "org.example.Other", t0);
        assert_eq!(s.apps(), ["org.example.Calc", "org.example.Edit"]);
        assert_eq!(s.since("org.example.CALC.desktop"), Some(t0 + Duration::from_secs(2)), "the newest launch");
        assert_eq!(s.since("org.example.Other"), None);
        // The end event clears it (one launch of two of the same app).
        assert_eq!(s.ended("t1").as_deref(), Some("org.example.Calc"));
        assert_eq!(s.ended("t1"), None, "once");
        assert_eq!(s.ended("unknown"), None);
        assert!(s.since("org.example.Calc").is_some(), "its other launch");
        assert_eq!(s.next_deadline(), Some(t0 + Duration::from_secs(17)));
        // The safety timeout clears what never ended.
        assert!(s.expire(t0 + Duration::from_secs(16)).is_empty());
        assert_eq!(s.expire(t0 + Duration::from_secs(17)), ["org.example.Edit"]);
        assert_eq!(s.apps(), ["org.example.calc"]);
        assert_eq!(s.expire(t0 + Duration::from_secs(60)), ["org.example.calc"]);
        assert!(s.is_empty() && s.next_deadline().is_none());
    }

    #[test]
    fn safety_timeout_is_over_the_compositors() {
        // (The test environment sets neither variable.)
        if std::env::var_os("HELIX_LAUNCH_TIMEOUT_MS").is_none() && std::env::var_os("HELIX_SHELL_LAUNCH_TIMEOUT_MS").is_none() {
            assert_eq!(safety_timeout(), Duration::from_secs(16));
        }
    }

    #[test]
    fn the_icon_breathes_or_stays_dimmed() {
        use helix_draw::motion::Motion;
        assert!(pulses(Motion::Full));
        assert!(!pulses(Motion::Reduced) && !pulses(Motion::None), "reduced motion: still");
        // Still: dimmed, whenever.
        for ms in [0, 400, 800, 5000] {
            assert_eq!(icon_alpha(Duration::from_millis(ms), false), STILL_ALPHA);
        }
        // Breathing: bright at first, dimmest half a period in, within range.
        assert!((icon_alpha(Duration::ZERO, true) - PULSE_MAX).abs() < 1e-6);
        assert!((icon_alpha(PULSE_PERIOD / 2, true) - PULSE_MIN).abs() < 1e-6);
        let samples: Vec<f32> = (0..64).map(|i| icon_alpha(PULSE_PERIOD * i / 32, true)).collect();
        assert!(samples.iter().all(|a| (PULSE_MIN - 1e-6..=PULSE_MAX + 1e-6).contains(a)));
        // Gentle: no jumps between frames.
        for w in (0..200).map(|i| icon_alpha(PULSE_FRAME * i, true)).collect::<Vec<_>>().windows(2) {
            assert!((w[0] - w[1]).abs() < 0.06, "{w:?}");
        }
    }
}
