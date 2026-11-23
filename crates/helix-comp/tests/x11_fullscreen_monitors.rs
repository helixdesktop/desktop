//! `_NET_WM_FULLSCREEN_MONITORS` (EWMH; #226), as mutter: an X11 app
//! names the monitors its full screen spans (top, bottom, left, right, by
//! Xinerama index), and while full screen it covers them: one monitor (a
//! game choosing which), or several (flight and racing sims across two),
//! with the shell's top-layer surfaces there hidden as for full screen on
//! one. Monitors that aren't there (a bad index, one unplugged) leave it
//! full screen on its own monitor. Only Helix's window manager
//! (`HELIX_XWM=helix`) does this; Smithay's doesn't advertise it.
//!
//! Two 800×600 monitors side by side (`HELIX_HEADLESS_OUTPUTS=2`). Which
//! Xinerama index each is comes from the X server, as an app would ask.

mod common;

use std::path::Path;
use std::time::{Duration, Instant};

use common::xwm::{session_env, until, X, SCREEN};
use common::Xwm;
use helix_test_support::headless::HeadlessSession;
use x11rb::protocol::xinerama::ConnectionExt as _;

const WHY: &str = "Smithay's window manager has no _NET_WM_FULLSCREEN_MONITORS (#226)";

fn two_monitors() -> Option<HeadlessSession> {
    session_env(vec![("HELIX_HEADLESS_OUTPUTS".to_owned(), "2".to_owned())])
}

/// The Xinerama indices of the left monitor and the right one.
fn indices(x: &X) -> (u32, u32) {
    let screens = x.conn.xinerama_query_screens().unwrap().reply().unwrap().screen_info;
    assert_eq!(screens.len(), 2, "two monitors in Xinerama: {screens:?}");
    let at = |left: i16| screens.iter().position(|s| s.x_org == left).expect("a monitor there") as u32;
    (at(0), at(SCREEN.0 as i16))
}

/// `_NET_WM_FULLSCREEN_MONITORS`: top, bottom, left, right.
fn name_monitors(x: &X, window: u32, [top, bottom, left, right]: [u32; 4]) {
    x.tell_wm(window, "_NET_WM_FULLSCREEN_MONITORS", [top, bottom, left, right, 1]);
}

fn geometry(x: &X, window: u32) -> (i16, i16, u16, u16) {
    let (px, py) = x.position(window);
    let (w, h) = x.size(window);
    (px, py, w, h)
}

/// Wait until the window's geometry is `want`, or fail with `what` and
/// where it is.
fn until_at(x: &X, window: u32, what: &str, want: impl Fn((i16, i16, u16, u16)) -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let now = geometry(x, window);
        if want(now) {
            return;
        }
        assert!(Instant::now() < deadline, "{what}: at {now:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The RGB at (x, y) of a snapshot, if it can be read now.
fn pixel(path: &Path, x: u32, y: u32) -> Option<u32> {
    let decoder = png::Decoder::new(std::io::BufReader::new(std::fs::File::open(path).ok()?));
    let mut reader = decoder.read_info().ok()?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader.next_frame(&mut buf).ok()?;
    let i = ((y * info.width + x) * 4) as usize;
    Some(u32::from(*buf.get(i)?) << 16 | u32::from(buf[i + 1]) << 8 | u32::from(buf[i + 2]))
}

/// What [`common::Client::panel`] draws.
const PANEL: u32 = 0x808080;

/// Wait (snapshots come every second) until both monitors' snapshots show
/// the panel at (`x`, `y`) or not.
fn panels_shown(session: &HeadlessSession, shown: bool, what: &str) {
    let paths = [session.snapshot_path(), session.home().join("snapshot-2.png")];
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let seen: Vec<Option<u32>> = paths.iter().map(|p| pixel(p, 400, 20)).collect();
        if seen.iter().all(|p| p.is_some_and(|p| (p == PANEL) == shown)) {
            return;
        }
        assert!(Instant::now() < deadline, "{what}: {seen:x?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Helix's window manager says it does it.
#[test]
fn it_is_supported() {
    if common::known_failure(&[Xwm::Smithay], WHY) {
        return;
    }
    let Some(session) = two_monitors() else { return };
    let x = X::new(&session);
    let supported = x.get32(x.root, "_NET_SUPPORTED").unwrap_or_default();
    assert!(supported.contains(&x.atom("_NET_WM_FULLSCREEN_MONITORS")), "_NET_FULLSCREEN_MONITORS is not in _NET_SUPPORTED");
}

/// One monitor named before going full screen (a game picking its
/// monitor): full screen goes there, not on the window's own, and the
/// property says so.
#[test]
fn one_monitor_named_before_full_screen() {
    if common::known_failure(&[Xwm::Smithay], WHY) {
        return;
    }
    let Some(session) = two_monitors() else { return };
    let x = X::new(&session);
    let (left, right) = indices(&x);
    let window = x.mapped();
    // The monitor it isn't on.
    let (other, at) = if x.position(window).0 < SCREEN.0 as i16 { (right, SCREEN.0 as i16) } else { (left, 0) };
    name_monitors(&x, window, [other; 4]);
    until("_NET_WM_FULLSCREEN_MONITORS was not set on the window", || x.get32(window, "_NET_WM_FULLSCREEN_MONITORS") == Some(vec![other; 4]));
    x.net_wm_state(window, 1, &["_NET_WM_STATE_FULLSCREEN"]);
    until("the window did not go full screen", || x.has_state(window, "_NET_WM_STATE_FULLSCREEN"));
    until_at(&x, window, "full screen is not on the monitor it named", |g| g == (at, 0, SCREEN.0, SCREEN.1));
}

/// Named before it is even shown, full screen from the start: it maps
/// spanning both monitors.
#[test]
fn monitors_named_before_mapping() {
    if common::known_failure(&[Xwm::Smithay], WHY) {
        return;
    }
    let Some(session) = two_monitors() else { return };
    let x = X::new(&session);
    let (left, right) = indices(&x);
    let window = x.window();
    // As EWMH has a client do it before mapping: the property, set itself.
    x.set_atoms(window, "_NET_WM_STATE", &["_NET_WM_STATE_FULLSCREEN"]);
    name_monitors(&x, window, [left, left, left, right]);
    until("_NET_WM_FULLSCREEN_MONITORS was not set on the window", || x.get32(window, "_NET_WM_FULLSCREEN_MONITORS") == Some(vec![left, left, left, right]));
    x.map_managed(window);
    until_at(&x, window, "the window did not map full screen across both monitors", |g| g == (0, 0, 2 * SCREEN.0, SCREEN.1));
}

/// Named while already full screen: it spreads over both monitors, and
/// the shell's panels on both are covered; named back to one, it shrinks
/// to it.
#[test]
fn spanning_two_monitors_while_full_screen() {
    if common::known_failure(&[Xwm::Smithay], WHY) {
        return;
    }
    let Some(session) = two_monitors() else { return };
    let x = X::new(&session);
    let (left, right) = indices(&x);
    let mut shell = common::connect(&session);
    assert_eq!(shell.outputs.len(), 2, "two wl_outputs");
    let outputs = shell.outputs.clone();
    let _panels: Vec<_> = outputs.iter().map(|o| shell.panel_on(40, Some(o))).collect();
    panels_shown(&session, true, "the panels are not shown on both monitors");
    let window = x.mapped();
    x.net_wm_state(window, 1, &["_NET_WM_STATE_FULLSCREEN"]);
    until("the window did not go full screen", || x.has_state(window, "_NET_WM_STATE_FULLSCREEN"));
    name_monitors(&x, window, [left, left, left, right]);
    until_at(&x, window, "the full-screen window does not span both monitors", |g| g == (0, 0, 2 * SCREEN.0, SCREEN.1));
    panels_shown(&session, false, "a panel shows over the full-screen window spanning both monitors");
    name_monitors(&x, window, [right; 4]);
    until_at(&x, window, "the full-screen window did not go to the one monitor named", |g| g == (SCREEN.0 as i16, 0, SCREEN.0, SCREEN.1));
    assert!(x.has_state(window, "_NET_WM_STATE_FULLSCREEN"), "no longer full screen");
}

/// Monitors that aren't there: an index past the last drops the list
/// (and the property), and a monitor unplugged while the window spans it
/// leaves the window full screen on the one it is on.
#[test]
fn missing_monitors_fall_back_to_the_windows_own() {
    if common::known_failure(&[Xwm::Smithay], WHY) {
        return;
    }
    let Some(mut session) = two_monitors() else { return };
    let x = X::new(&session);
    let (left, right) = indices(&x);
    let window = x.mapped();
    x.net_wm_state(window, 1, &["_NET_WM_STATE_FULLSCREEN"]);
    name_monitors(&x, window, [left, left, left, right]);
    until_at(&x, window, "the full-screen window does not span both monitors", |g| g == (0, 0, 2 * SCREEN.0, SCREEN.1));
    // No such monitor.
    name_monitors(&x, window, [left, left, left, 7]);
    until("_NET_WM_FULLSCREEN_MONITORS naming no monitor was kept", || x.get32(window, "_NET_WM_FULLSCREEN_MONITORS").is_none());
    until_at(&x, window, "the full-screen window is not back on one monitor", |(px, py, w, h)| (px == 0 || px == SCREEN.0 as i16) && (py, w, h) == (0, SCREEN.0, SCREEN.1));
    // Spanning again, then the right monitor goes.
    name_monitors(&x, window, [left, left, left, right]);
    until_at(&x, window, "the full-screen window does not span both monitors again", |g| g == (0, 0, 2 * SCREEN.0, SCREEN.1));
    session.input("unplug HEADLESS-2");
    until_at(&x, window, "the full-screen window still spans the unplugged monitor", |g| g == (0, 0, SCREEN.0, SCREEN.1));
    assert!(x.has_state(window, "_NET_WM_STATE_FULLSCREEN"), "no longer full screen after the unplug");
}
