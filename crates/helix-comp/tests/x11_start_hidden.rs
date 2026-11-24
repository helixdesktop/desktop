//! X11 windows that start hidden (#288): asking to start minimized
//! (`WM_HINTS` Iconic), on another workspace (`_NET_WM_DESKTOP` before
//! mapping, #279), or both. As mutter, the window manager decides that
//! before it first shows the window, so nothing sees it shown first: the
//! app never gets `WM_STATE` Normal, `_NET_WM_STATE_FOCUSED`, the X focus
//! or the active workspace's number, the window that has the focus keeps
//! it (no FocusOut, `_NET_ACTIVE_WINDOW` unchanged), and the shell never
//! sees it focused, shown or on the active workspace.
//!
//! What the window manager writes is followed as it writes it (the X
//! RECORD extension, `common::record`), so a state put right straight
//! after counts too.

mod common;

use std::time::Duration;

use common::record::Recorder;
use common::xwm::{session_with_shell_settings, until, Shell, X};
use common::Xwm;
use helix_test_support::headless::HeadlessSession;
use x11rb::properties::{WmHints, WmHintsState};
use x11rb::protocol::xproto::{AtomEnum, Window};
use x11rb::protocol::Event;

const WHY: &str = "Smithay's window manager ignores the workspace a window asks to start on (#279)";

/// Four static workspaces, so that the numbers don't depend on which are
/// in use.
const SETTINGS: &str = "[org/gnome/mutter]\ndynamic-workspaces=false\n\n[org/gnome/desktop/wm/preferences]\nnum-workspaces=4\n";

/// A session with a focused X11 window ("shown") on the first workspace,
/// and the test as its shell.
struct Desk {
    session: HeadlessSession,
    shell: Shell,
    x: X,
    shown: Window,
    shown_id: u32,
}

fn desk(name: &str) -> Option<Desk> {
    let (session, mut shell) = session_with_shell_settings(name, Vec::new(), SETTINGS)?;
    let x = X::new(&session);
    let shown = x.mapped_titled("shown");
    x.focus(shown);
    assert_eq!(x.input_focus(), shown, "the shown window never got the X focus");
    let shown_id = shell.window("shown").id;
    shell.until("the shown window is not focused in the window list", |s| s.focused == Some(shown_id));
    Some(Desk { session, shell, x, shown, shown_id })
}

impl Desk {
    /// Start following what the window manager does and the shell sees,
    /// from now on.
    fn watch(&mut self) -> Recorder {
        self.x.sync();
        self.x.drain();
        self.shell.roundtrip();
        self.shell.state.focus_log.clear();
        self.shell.state.workspace_log.clear();
        self.shell.state.minimized_log.clear();
        Recorder::start(&self.session, &self.x.conn)
    }

    /// A window titled `title`, not mapped yet, that takes part in
    /// `WM_TAKE_FOCUS` (Java, Qt and Wine windows do): a window manager
    /// giving it the focus sends it that, mapped or not.
    fn window(&self, title: &str) -> Window {
        let window = self.x.window();
        self.x.set8(window, "_NET_WM_NAME", self.x.atom("UTF8_STRING"), title.as_bytes());
        self.x.set_atoms(window, "WM_PROTOCOLS", &["WM_DELETE_WINDOW", "WM_TAKE_FOCUS"]);
        window
    }

    /// `window` (listed as `id`) never had the focus or looked like it
    /// had, and the shown window kept it, all along.
    fn never_focused(&mut self, record: &Recorder, window: Window, id: u32) {
        let x = &self.x;
        x.sync();
        assert!(!record.focused(window), "the window was given the X focus");
        let active = record.values(x.root, x.atom("_NET_ACTIVE_WINDOW"));
        assert!(active.iter().all(|v| v.first() == Some(&self.shown)), "_NET_ACTIVE_WINDOW changed while the window opened: {active:?}");
        let focused = x.atom("_NET_WM_STATE_FOCUSED");
        let states = record.values(window, x.atom("_NET_WM_STATE"));
        assert!(!states.iter().any(|s| s.contains(&focused)), "the window was told it was focused (_NET_WM_STATE): {states:?}");
        let states = record.values(self.shown, x.atom("_NET_WM_STATE"));
        assert!(states.iter().all(|s| s.contains(&focused)), "the focused window was told it no longer was (_NET_WM_STATE): {states:?}");
        let events = x.events_for(Duration::from_millis(300));
        let take_focus = x.atom("WM_TAKE_FOCUS");
        let asked = events.iter().any(|e| matches!(e, Event::ClientMessage(m) if m.window == window && m.data.as_data32()[0] == take_focus));
        let focus_in = events.iter().any(|e| matches!(e, Event::FocusIn(f) if f.event == window));
        let focus_out = events.iter().any(|e| matches!(e, Event::FocusOut(f) if f.event == self.shown));
        assert!(!asked, "the window was asked to take the focus (WM_TAKE_FOCUS)");
        assert!(!focus_in, "the window got a FocusIn: {events:?}");
        assert!(!focus_out, "the focused window got a FocusOut: {events:?}");
        assert_eq!(x.input_focus(), self.shown, "the focused window lost the X focus");
        self.shell.roundtrip();
        let log = &self.shell.state.focus_log;
        assert!(log.iter().all(|f| *f == Some(self.shown_id)), "the shell saw the focus move: {log:?} (the window is {id})");
        assert_eq!(self.shell.state.focused, Some(self.shown_id));
    }

    /// The shell only ever saw window `id` minimized.
    fn always_minimized(&mut self, id: u32) {
        self.shell.until("the window is not listed minimized", |s| s.windows.iter().any(|w| w.id == id && w.minimized));
        let log: Vec<bool> = self.shell.state.minimized_log.iter().filter(|(i, _)| *i == id).map(|(_, m)| *m).collect();
        assert!(log.iter().all(|m| *m), "the shell saw the window shown before minimized: {log:?}");
    }

    /// The shell only ever saw window `id` on workspace `n`.
    fn always_on(&mut self, id: u32, n: u32) {
        self.shell.until("the window is not listed on its workspace", |s| s.workspace_log.contains(&(id, n)));
        let log: Vec<u32> = self.shell.state.workspace_log.iter().filter(|(i, _)| *i == id).map(|(_, w)| *w).collect();
        assert!(log.iter().all(|w| *w == n), "the shell saw the window on another workspace first: {log:?}");
    }
}

fn set_iconic(x: &X, window: Window) {
    let mut hints = WmHints::new();
    hints.initial_state = Some(WmHintsState::Iconic);
    hints.set(&x.conn, window).unwrap();
}

/// Every `WM_STATE` the window manager gave `window` says Iconic.
fn only_iconic(x: &X, record: &Recorder, window: Window) {
    let states = record.values(window, x.atom("WM_STATE"));
    assert!(!states.is_empty() && states.iter().all(|s| s.first() == Some(&3)), "WM_STATE was not only ever Iconic: {states:?}");
}

/// Every `_NET_WM_DESKTOP` the window manager gave `window` is `n`.
fn only_desktop(x: &X, record: &Recorder, window: Window, n: u32) {
    let desktops = record.values(window, x.atom("_NET_WM_DESKTOP"));
    assert!(!desktops.is_empty() && desktops.iter().all(|d| d == &[n]), "_NET_WM_DESKTOP was not only ever {n}: {desktops:?}");
}

/// Asked to start minimized: `WM_STATE` Iconic from the start, never
/// Normal, and never focused.
#[test]
fn a_window_starting_iconic_is_never_normal_or_focused() {
    let Some(mut d) = desk("x11-start-iconic") else { return };
    let record = d.watch();
    let window = d.window("iconic");
    set_iconic(&d.x, window);
    d.x.map(window);
    until("the window is not Iconic", || d.x.wm_state(window) == Some(3));
    let id = d.shell.window("iconic").id;
    d.always_minimized(id);
    only_iconic(&d.x, &record, window);
    d.never_focused(&record, window, id);
    assert!(d.x.has_state(window, "_NET_WM_STATE_HIDDEN"), "_NET_WM_STATE_HIDDEN is not set");
}

/// Asked to start on workspace 2 (not the active one): parked there from
/// the start, never focused, and never said to be on the active one.
#[test]
fn a_window_starting_on_another_workspace_is_never_focused_or_here() {
    if common::known_failure(&[Xwm::Smithay], WHY) {
        return;
    }
    let Some(mut d) = desk("x11-start-elsewhere") else { return };
    until("there are not four workspaces", || d.x.get32(d.x.root, "_NET_NUMBER_OF_DESKTOPS") == Some(vec![4]));
    let record = d.watch();
    let window = d.window("elsewhere");
    d.x.set32(window, "_NET_WM_DESKTOP", AtomEnum::CARDINAL, &[2]);
    d.x.map(window);
    until("the window is not managed", || d.x.managed(window));
    let id = d.shell.window("elsewhere").id;
    d.always_on(id, 2);
    only_desktop(&d.x, &record, window, 2);
    d.never_focused(&record, window, id);
    assert_eq!(d.x.get32(d.x.root, "_NET_CURRENT_DESKTOP"), Some(vec![0]), "the workspace changed");
}

/// Both: minimized on workspace 2 from the start, and still minimized when
/// that workspace is shown.
#[test]
fn a_window_starting_iconic_on_another_workspace_is_minimized_there() {
    if common::known_failure(&[Xwm::Smithay], WHY) {
        return;
    }
    let Some(mut d) = desk("x11-start-iconic-elsewhere") else { return };
    until("there are not four workspaces", || d.x.get32(d.x.root, "_NET_NUMBER_OF_DESKTOPS") == Some(vec![4]));
    let record = d.watch();
    let window = d.window("iconic elsewhere");
    set_iconic(&d.x, window);
    d.x.set32(window, "_NET_WM_DESKTOP", AtomEnum::CARDINAL, &[2]);
    d.x.map(window);
    until("the window is not Iconic", || d.x.wm_state(window) == Some(3));
    let id = d.shell.window("iconic elsewhere").id;
    d.always_minimized(id);
    d.always_on(id, 2);
    until("the window is not said to be on workspace 2", || d.x.get32(window, "_NET_WM_DESKTOP") == Some(vec![2]));
    only_iconic(&d.x, &record, window);
    only_desktop(&d.x, &record, window, 2);
    d.never_focused(&record, window, id);
    d.shell.switch_workspace(2);
    until("workspace 2 was not shown", || d.x.get32(d.x.root, "_NET_CURRENT_DESKTOP") == Some(vec![2]));
    d.x.sync();
    assert_eq!(d.x.wm_state(window), Some(3), "the minimized window was shown with its workspace");
    d.shell.until("the window is not listed minimized on its workspace", |s| s.windows.iter().any(|w| w.id == id && w.minimized));
}
