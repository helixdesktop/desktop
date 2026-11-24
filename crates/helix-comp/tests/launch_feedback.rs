//! Launch feedback (`tests/x11_cases.md` X89; helixdesktop/spec
//! `contracts/wayland/README.md`, "Launch feedback"; #291): while an app
//! the shell launched is starting, the pointer shows the "progress" cursor
//! over the desktop and the shell (over app windows, theirs). The shell
//! says a token is a launch's with `gtk_shell1.notify_launch`; the launch
//! ends when the token is used, a window of the app maps, the X11 app sends
//! `_NET_STARTUP_INFO` "remove:", or after a timeout. An X11 launcher's
//! "new:" starts one too. Under either window manager (`HELIX_XWM`); the
//! shell is the test, the pointer image is the test input socket's
//! `cursor` answer.
//!
//! The shell hears when each launch it announced ends
//! (`helix_window_list_v1.launch_ended`, with why: #298), and of no other
//! launches.

mod common;

use std::time::{Duration, Instant};

use common::xwm::{self, Shell, X};
use helix_test_support::headless::HeadlessSession;
use x11rb::protocol::xproto::{AtomEnum, ClientMessageEvent, ConnectionExt, EventMask};

/// Where the pointer waits: on the desktop, away from where windows go
/// (and from the hot corner).
const DESKTOP: (u32, u32) = (795, 595);

fn session(name: &str, timeout_ms: Option<u64>) -> Option<(HeadlessSession, Shell)> {
    let env = timeout_ms.map(|ms| ("HELIX_LAUNCH_TIMEOUT_MS".to_owned(), ms.to_string())).into_iter().collect();
    let (mut session, shell) = xwm::session_with_shell_env(name, env)?;
    session.input(&format!("move {} {}", DESKTOP.0, DESKTOP.1));
    Some((session, shell))
}

/// Wait (up to 5 s) until the pointer shows `cursor`.
fn cursor_becomes(session: &mut HeadlessSession, cursor: &str, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let shown = session.query("cursor");
        if shown == cursor {
            return;
        }
        assert!(Instant::now() < deadline, "{what}: the pointer shows {shown:?}, not {cursor:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The pointer shows `cursor` now and for a while.
fn cursor_stays(session: &mut HeadlessSession, cursor: &str, what: &str) {
    for _ in 0..10 {
        let shown = session.query("cursor");
        assert_eq!(shown, cursor, "{what}");
        std::thread::sleep(Duration::from_millis(30));
    }
}

/// `helix_window_list_v1.launch_end` values.
const USED: u32 = 0;
const WINDOW: u32 = 1;
const REMOVED: u32 = 2;
const TIMED_OUT: u32 = 3;

/// Wait until the shell has been told of exactly the launch ends `ended`
/// (startup id, reason), in order; then that no more come for a while.
fn shell_told(shell: &mut Shell, ended: &[(&str, u32)], what: &str) {
    let want: Vec<(String, u32)> = ended.iter().map(|(id, r)| ((*id).to_owned(), *r)).collect();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        shell.roundtrip();
        if shell.state.launches_ended.len() >= want.len() {
            break;
        }
        assert!(Instant::now() < deadline, "{what}: the shell was told {:?}, not {want:?}", shell.state.launches_ended);
        std::thread::sleep(Duration::from_millis(20));
    }
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(30));
        shell.roundtrip();
    }
    assert_eq!(shell.state.launches_ended, want, "{what}");
}

/// Send startup notification `message` as libstartup-notification does:
/// from a window of the sender's, in 20-byte pieces up to a NUL, to the
/// root window with PropertyChangeMask.
fn send_startup_info(x: &X, message: &str) {
    let sender = x.window();
    let (begin, more) = (x.atom("_NET_STARTUP_INFO_BEGIN"), x.atom("_NET_STARTUP_INFO"));
    let mut bytes = message.as_bytes().to_vec();
    bytes.push(0);
    for (i, chunk) in bytes.chunks(20).enumerate() {
        let mut data = [0u8; 20];
        data[..chunk.len()].copy_from_slice(chunk);
        let event = ClientMessageEvent::new(8, sender, if i == 0 { begin } else { more }, data);
        x.conn.send_event(false, x.root, EventMask::PROPERTY_CHANGE, event).unwrap();
    }
    x.flush();
}

/// Ordinary tokens are not launches; a launch's token shows the busy
/// pointer over the desktop (an app window's own cursor over the app)
/// until a window is activated with it.
#[test]
fn a_launch_is_busy_until_its_token_is_used() {
    let Some((mut session, mut shell)) = session("launch-token", None) else { return };
    cursor_stays(&mut session, "default", "nothing launched");
    let _focus_passing = shell.token();
    cursor_stays(&mut session, "default", "a token that is not a launch's");

    let token = shell.launch_token(Some("org.example.Launched.desktop"));
    cursor_becomes(&mut session, "progress", "an app launched");

    // Another app's window maps: still busy over the desktop; over the
    // window, its cursor.
    let mut app = common::connect(&session);
    let (window, ()) = app.toplevel((200, 120), |_, t| t.toplevel.set_app_id("org.example.Other".into()));
    cursor_stays(&mut session, "progress", "another app's window mapped");
    let (x, y) = common::point_over(&mut session, &mut app, &window.surface, (xwm::SCREEN.0.into(), xwm::SCREEN.1.into()));
    session.input(&format!("move {x} {y}"));
    cursor_stays(&mut session, "default", "over an app window");
    session.input(&format!("move {} {}", DESKTOP.0, DESKTOP.1));
    cursor_becomes(&mut session, "progress", "back on the desktop");

    // The launched app activates its window with the token (as a
    // single-instance app handing the launch to its running copy does).
    shell_told(&mut shell, &[], "nothing ended yet");
    app.activate_token(token.clone(), &window.surface);
    cursor_becomes(&mut session, "default", "the token was used");
    shell_told(&mut shell, &[(&token, USED)], "the token was used");
}

/// Launches overlap: busy until both ended. One ends when a window of its
/// app maps without using the token.
#[test]
fn overlapping_launches_and_windows_of_the_app() {
    let Some((mut session, mut shell)) = session("launch-overlap", None) else { return };
    let first = shell.launch_token(Some("org.example.First.desktop"));
    let second_token = shell.launch_token(Some("org.example.Second.desktop"));
    cursor_becomes(&mut session, "progress", "two apps launched");

    let mut app = common::connect(&session);
    let (window, ()) = app.toplevel((200, 120), |_, t| t.toplevel.set_app_id("org.example.First".into()));
    app.activate_token(first.clone(), &window.surface);
    cursor_stays(&mut session, "progress", "one launch of two ended");
    // (Its window, of the launched app, mapped first: that ended it.)
    shell_told(&mut shell, &[(&first, WINDOW)], "the first app's window mapped");

    // The second app never uses its token; its window maps.
    let mut second = common::connect(&session);
    let _window = second.toplevel((200, 120), |_, t| t.toplevel.set_app_id("org.example.Second".into()));
    cursor_becomes(&mut session, "default", "the second app's window mapped");
    shell_told(&mut shell, &[(&first, WINDOW), (&second_token, WINDOW)], "the second app's window mapped");
}

/// An X11 app launched with the token as `DESKTOP_STARTUP_ID` ends the
/// launch with `_NET_STARTUP_INFO` "remove:" (quoted, in several pieces)
/// without showing a window: long before the timeout.
#[test]
fn an_x11_remove_message_ends_the_launch() {
    let Some((mut session, mut shell)) = session("launch-x11-remove", Some(60_000)) else { return };
    let x = X::new(&session);
    let token = shell.launch_token(Some("org.example.Launched.desktop"));
    cursor_becomes(&mut session, "progress", "an app launched");
    // Another launch's "remove:" does nothing.
    send_startup_info(&x, "remove: ID=not-this-launch");
    cursor_stays(&mut session, "progress", "another launch removed");
    let sent = Instant::now();
    send_startup_info(&x, &format!("remove: ID=\"{token}\""));
    cursor_becomes(&mut session, "default", "the app removed its launch");
    assert!(sent.elapsed() < Duration::from_secs(10), "ended by the timeout, not the message");
    shell_told(&mut shell, &[(&token, REMOVED)], "the app removed its launch");
}

/// An X11 window mapped with the token as its `_NET_STARTUP_ID` ends the
/// launch (whatever its class).
#[test]
fn an_x11_window_with_the_token_ends_the_launch() {
    let Some((mut session, mut shell)) = session("launch-x11-id", Some(60_000)) else { return };
    let x = X::new(&session);
    let token = shell.launch_token(Some("org.example.Launched.desktop"));
    cursor_becomes(&mut session, "progress", "an app launched");
    let window = x.window();
    x.set8(window, "_NET_STARTUP_ID", x.atom("UTF8_STRING"), token.as_bytes());
    x.set8(window, "_NET_WM_NAME", x.atom("UTF8_STRING"), b"launched");
    x.map_managed(window);
    shell.window("launched");
    cursor_becomes(&mut session, "default", "the window with the launch's startup id mapped");
    shell_told(&mut shell, &[(&token, WINDOW)], "the window with the launch's startup id mapped");
}

/// An X11 launcher's "new:" starts a launch (as in mutter); a window whose
/// `WM_CLASS` is the message's `WMCLASS` ends it.
#[test]
fn an_x11_launchers_launch_ends_with_a_window_of_its_class() {
    let Some((mut session, mut shell)) = session("launch-x11-new", Some(60_000)) else { return };
    let x = X::new(&session);
    send_startup_info(&x, "new: ID=xterm-4242-host-xterm-0_TIME1234 NAME=\"X Term\" SCREEN=0 WMCLASS=XTerm");
    cursor_becomes(&mut session, "progress", "an X11 launcher's launch");
    let window = x.window();
    x.set8(window, "WM_CLASS", AtomEnum::STRING, b"xterm\0XTerm\0");
    x.set8(window, "_NET_WM_NAME", x.atom("UTF8_STRING"), b"xterm");
    x.map_managed(window);
    shell.window("xterm");
    cursor_becomes(&mut session, "default", "a window of the launched class mapped");

    // "remove:" ends one as well.
    send_startup_info(&x, "new: ID=other-1_TIME1");
    cursor_becomes(&mut session, "progress", "another launch");
    send_startup_info(&x, "remove: ID=other-1_TIME1");
    cursor_becomes(&mut session, "default", "removed");
    x.conn.destroy_window(window).unwrap();
    x.flush();
    // Not the shell's launches: it hears of neither.
    shell_told(&mut shell, &[], "X11 launchers' launches");
}

/// A launch nothing ends times out.
#[test]
fn a_launch_times_out() {
    let Some((mut session, mut shell)) = session("launch-timeout", Some(1500)) else { return };
    let started = Instant::now();
    let token = shell.launch_token(Some("org.example.Never.desktop"));
    cursor_becomes(&mut session, "progress", "an app launched");
    cursor_becomes(&mut session, "default", "the launch timed out");
    assert!(started.elapsed() >= Duration::from_millis(1400), "it ended before the timeout");
    shell_told(&mut shell, &[(&token, TIMED_OUT)], "the launch timed out");
}

/// A launch another client announced (any client may) is not the shell's:
/// when it ends, only the shell's own launch is reported to the shell.
#[test]
fn the_shell_hears_only_of_its_own_launches() {
    let Some((mut session, mut shell)) = session("launch-others", Some(1500)) else { return };
    let mut other = common::connect(&session);
    let theirs = other.token(None, None);
    other.gtk_shell.notify_launch(theirs.clone());
    other.roundtrip();
    cursor_becomes(&mut session, "progress", "another client's launch");
    let mine = shell.launch_token(Some("org.example.Mine.desktop"));
    // Both time out; the shell is told of its own only.
    cursor_becomes(&mut session, "default", "both launches timed out");
    shell_told(&mut shell, &[(&mine, TIMED_OUT)], "both launches timed out");
    assert!(theirs != mine);
}
