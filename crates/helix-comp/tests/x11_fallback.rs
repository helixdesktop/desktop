//! The X11 window manager `HELIX_XWM` asks for fails to start (#270):
//! Xwayland is started again on the same display with the other one, which
//! stays for the rest of the session, and X11 apps run.
//!
//! The failure is injected with `HELIX_XWM_FAIL_START=1` (debug builds
//! only): Helix's window manager fails partway through becoming the window
//! manager, Smithay's before it begins.

mod common;

use common::xwm::{kill_xwayland, restarted_xwayland, session_env, X};
use common::{xwm_running, Xwm};

/// The name the running window manager gives itself
/// (`_NET_SUPPORTING_WM_CHECK`'s `_NET_WM_NAME`).
fn wm_name(x: &X) -> String {
    let check = x.get32(x.root, "_NET_SUPPORTING_WM_CHECK").and_then(|v| v.first().copied()).expect("a window manager check window");
    let (_, name) = x.get8(check, "_NET_WM_NAME").expect("the window manager's name");
    String::from_utf8_lossy(&name).into_owned()
}

/// How many Xwayland processes serve `display` (`Xwayland :N ...`).
fn xwaylands(display: &str) -> usize {
    let Ok(dir) = std::fs::read_dir("/proc") else { return 0 };
    dir.flatten()
        .filter(|entry| {
            let Ok(cmdline) = std::fs::read(entry.path().join("cmdline")) else { return false };
            let mut args = cmdline.split(|b| *b == 0);
            args.next().is_some_and(|p| p.ends_with(b"Xwayland")) && args.next() == Some(display.as_bytes())
        })
        .count()
}

#[test]
fn x11_apps_run_when_the_chosen_window_manager_fails_to_start() {
    let env = vec![("HELIX_XWM_FAIL_START".to_owned(), "1".to_owned())];
    let Some(session) = session_env(env) else { return };
    let (other, other_name) = match xwm_running() {
        Xwm::Helix => ("Smithay's X11 window manager", "Smithay X WM"),
        Xwm::Smithay => ("Helix's X11 window manager", "Helix"),
    };

    // The log says why, and which window manager took over, on which
    // display: the one the session was given (DISPLAY unchanged).
    let log = session.log();
    let line = log.lines().find(|l| l.contains("failed to start") && l.contains(&format!("falling back to {other}"))).unwrap_or_else(|| panic!("no fallback in the log:\n{log}"));
    assert!(line.contains("HELIX_XWM_FAIL_START"), "the reason is logged: {line}");
    let display = session.x_display.trim_start_matches(':');
    assert!(line.contains(&format!("display={display}")), "fell back on the session's display {}: {line}", session.x_display);
    assert!(log.contains(&format!("xwm=\"{other}\"")) || log.contains(&format!("xwm={other}")), "Xwayland ready with {other}:\n{log}");

    // The Xwayland the failed one ran on is gone: one X server on the
    // display.
    common::xwm::until("the first Xwayland exits", || xwaylands(&session.x_display) == 1);

    // X11 apps map and are managed, by the other window manager.
    let x = X::new(&session);
    assert_eq!(wm_name(&x), other_name);
    let window = x.mapped();
    assert!(x.client_list().contains(&window), "the window is listed");
    drop(x);

    // Remembered: Xwayland started again later keeps the other one.
    let killed = kill_xwayland(&session);
    let x = restarted_xwayland(&session, killed);
    assert_eq!(wm_name(&x), other_name);
    x.mapped();
}
