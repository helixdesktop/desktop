//! Drag and drop between X11 apps (through Xwayland) and Wayland apps
//! (#170), as the apps see it:
//!
//! - An X11 app drags (XDND) onto a Wayland window: the Wayland app gets
//!   the offer and the data (an image from GIMP into Files).
//! - A Wayland app drags onto an X11 window: the X11 app gets XDND and
//!   the data (files from Files into a Wine or Java app).
//! - An X11 app drags onto another X11 app's window, still straight
//!   between them (Smithay's window manager steps aside).
//!
//! And the drag-and-drop edge cases of `tests/x11_cases.md` (#191):
//! sources that message from another window, Chromium-style target
//! picking, XDND proxies, drag icons under the pointer, sources and
//! targets going away mid-drag, refused and forged XdndFinished, late
//! and concurrent fetches, and a drag crossing several X11 windows.
//!
//! Runs the real `helix-comp` headless; needs `dbus-daemon` and Xwayland
//! (as every headless session), skipped otherwise.

mod common;

use std::time::{Duration, Instant};

use common::xdnd::{expect, glide, point_in_not, session, xdnd_source, xdnd_source_with, xdnd_target_app, xdnd_target_with, message, message_as, Source, SourceSaw, Target, TargetSaw, X11, BTN_LEFT, DATA, XDND_VERSION};
use helix_test_support::headless::HeadlessSession;
use wayland_client::protocol::wl_data_device_manager::DndAction;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, CreateWindowAux, EventMask, PropMode, WindowClass};
use x11rb::protocol::Event;
use x11rb::wrapper::ConnectionExt as _;

#[test]
fn an_x11_app_drags_onto_a_wayland_window() {
    let Some(mut session) = session() else { return };
    let mut app = common::connect(&session);
    let (window, ()) = app.toplevel((300, 200), |_, _| {});
    let surface = window.surface.clone();
    let x = X11::new(&session, (200, 150), false);
    let x_rect = x.rect();
    // Over the Wayland window where the X11 one doesn't cover it.
    let over_app = common::point_over(&mut session, &mut app, &surface, (800, 600));
    let start = point_in_not(x_rect, None);
    let saw = xdnd_source(x);

    session.input(&format!("move {} {}", start.0, start.1));
    std::thread::sleep(Duration::from_millis(200));
    session.input(&format!("button {BTN_LEFT} 1"));
    std::thread::sleep(Duration::from_millis(300));
    glide(&mut session, start, over_app);

    expect(&saw, |s| matches!(s, SourceSaw::Target(_)), "the X11 drag never found a target over the Wayland window");
    app.until("the Wayland window was never offered the drag", |s| s.dnd.is_some());
    let (offer, types, serial) = app.state.dnd.clone().unwrap();
    assert!(types.iter().any(|t| t == "text/plain;charset=utf-8"), "offered {types:?}");
    offer.accept(serial, Some("text/plain;charset=utf-8".to_owned()));
    offer.set_actions(DndAction::Copy, DndAction::Copy);
    app.roundtrip();
    // One more move, for the source to hear it is accepted.
    session.input(&format!("move {} {}", over_app.0 + 1, over_app.1 + 1));
    expect(&saw, |s| matches!(s, SourceSaw::Accepted(_)), "the X11 source never heard the Wayland window accept");

    session.input(&format!("button {BTN_LEFT} 0"));
    app.until("the drag was never dropped on the Wayland window", |s| s.dropped);
    assert_eq!(app.receive(&offer, "text/plain;charset=utf-8"), DATA);
    offer.finish();
    app.roundtrip();
    expect(&saw, |s| matches!(s, SourceSaw::Finished(..)), "the X11 source never heard the drop finished");
}

#[test]
fn a_wayland_app_drags_onto_an_x11_window() {
    let Some(mut session) = session() else { return };
    let x = X11::new(&session, (300, 200), true);
    let x_rect = x.rect();
    let mut app = common::connect(&session);
    let (window, ()) = app.toplevel((200, 150), |_, _| {});
    let surface = window.surface.clone();
    // Over the Wayland window (on top), then over the X11 window where
    // the Wayland one doesn't cover it.
    let start = common::point_over(&mut session, &mut app, &surface, (800, 600));
    let over_x = (x_rect.0 + 10..x_rect.0 + x_rect.2 - 10)
        .step_by(10)
        .flat_map(|px| (x_rect.1 + 10..x_rect.1 + x_rect.3 - 10).step_by(10).map(move |py| (px, py)))
        .find(|(px, py)| {
            session.input(&format!("move {px} {py}"));
            app.roundtrip();
            app.state.pointer.is_none()
        })
        .expect("the Wayland window covers the X11 one");
    let saw = xdnd_target_app(x);

    session.input(&format!("move {} {}", start.0, start.1));
    app.until("the pointer never came back to the Wayland window", |s| s.pointer.as_ref() == Some(&surface));
    session.input(&format!("button {BTN_LEFT} 1"));
    app.until("the Wayland window never got the press", |s| s.press.is_some());
    let press = app.state.press.unwrap();
    app.drag("text/plain;charset=utf-8", DATA, &surface, press);
    glide(&mut session, start, over_x);

    expect(&saw, |s| matches!(s, TargetSaw::Entered(..)), "the X11 window never got XdndEnter");
    session.input(&format!("move {} {}", over_x.0 + 1, over_x.1 + 1));
    std::thread::sleep(Duration::from_millis(200));
    session.input(&format!("button {BTN_LEFT} 0"));
    // The Wayland source serves the data while the X11 app fetches it.
    let deadline = Instant::now() + Duration::from_secs(5);
    let got = loop {
        app.roundtrip();
        match saw.try_recv() {
            Ok(TargetSaw::Dropped(data)) => break data,
            Ok(_) => {}
            Err(_) => {
                assert!(Instant::now() < deadline, "the X11 window was never dropped on");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    };
    assert_eq!(got, DATA);
}

#[test]
fn an_x11_app_drags_onto_another_x11_window() {
    let Some(mut session) = session() else { return };
    // Crossed, so that each has a part the other doesn't cover whichever
    // is on top (the press raises the source).
    let source = X11::new(&session, (300, 80), false);
    let target = X11::new(&session, (80, 300), true);
    let (source_rect, target_rect) = (source.rect(), target.rect());
    let start = point_in_not(source_rect, Some(target_rect));
    let over_target = point_in_not(target_rect, Some(source_rect));
    let target_window = target.window;
    // The pointer over each first, so Xwayland knows where they are.
    session.input(&format!("move {} {}", over_target.0, over_target.1));
    std::thread::sleep(Duration::from_millis(200));
    let saw = xdnd_source(source);
    let got = xdnd_target_app(target);

    session.input(&format!("move {} {}", start.0, start.1));
    std::thread::sleep(Duration::from_millis(200));
    session.input(&format!("button {BTN_LEFT} 1"));
    std::thread::sleep(Duration::from_millis(300));
    glide(&mut session, start, over_target);

    // Smithay's drop proxy covers the screen until the drag is over an
    // X11 window; then the source finds the window itself.
    session.input(&format!("move {} {}", over_target.0 + 2, over_target.1 + 2));
    expect(&saw, |s| *s == SourceSaw::Target(target_window), "the X11 drag never found the X11 window (only Smithay's drop proxy?)");
    expect(&got, |s| matches!(s, TargetSaw::Entered(..)), "the X11 target never got XdndEnter");
    session.input(&format!("move {} {}", over_target.0 + 1, over_target.1 + 1));
    std::thread::sleep(Duration::from_millis(200));
    session.input(&format!("button {BTN_LEFT} 0"));
    assert_eq!(expect(&got, |s| matches!(s, TargetSaw::Dropped(_)), "the X11 target was never dropped on"), TargetSaw::Dropped(DATA.to_vec()));
    expect(&saw, |s| matches!(s, SourceSaw::Finished(..)), "the X11 source never heard the drop finished");
}

/// An X11 app's drag over a Wayland window, up to the Wayland app's
/// offer: the source's reports, the Wayland app, where the pointer is
/// over its window, and the window itself.
struct XToWayland {
    saw: std::sync::mpsc::Receiver<SourceSaw>,
    app: common::Client,
    surface: wayland_client::protocol::wl_surface::WlSurface,
    over_app: (i32, i32),
    /// The window the source sent its positions to.
    target: u32,
}

/// Start an X11 app's drag (`make` gives the app and how its source
/// behaves) from its window onto a Wayland window, until the Wayland app
/// is offered it.
fn x11_drag_to_wayland(session: &mut HeadlessSession, make: impl FnOnce(&HeadlessSession) -> (X11, Source)) -> XToWayland {
    let mut app = common::connect(session);
    let (window, ()) = app.toplevel((300, 200), |_, _| {});
    let surface = window.surface.clone();
    std::mem::forget(window);
    let (x, how) = make(session);
    let x_rect = x.rect();
    let over_app = common::point_over(session, &mut app, &surface, (800, 600));
    let start = point_in_not(x_rect, None);
    let saw = xdnd_source_with(x, how);

    session.input(&format!("move {} {}", start.0, start.1));
    std::thread::sleep(Duration::from_millis(200));
    session.input(&format!("button {BTN_LEFT} 1"));
    std::thread::sleep(Duration::from_millis(300));
    glide(session, start, over_app);
    let SourceSaw::Target(target) = expect(&saw, |s| matches!(s, SourceSaw::Target(_)), "the X11 drag never found a target over the Wayland window") else { unreachable!() };
    app.until("the Wayland window was never offered the drag", |s| s.dnd.is_some());
    XToWayland { saw, app, surface, over_app, target }
}

impl XToWayland {
    /// The Wayland app accepts the text, and the source hears it.
    fn accept(&mut self, session: &mut HeadlessSession) -> wayland_client::protocol::wl_data_offer::WlDataOffer {
        let (offer, types, serial) = self.app.state.dnd.clone().unwrap();
        assert!(types.iter().any(|t| t == "text/plain;charset=utf-8"), "offered {types:?}");
        offer.accept(serial, Some("text/plain;charset=utf-8".to_owned()));
        offer.set_actions(DndAction::Copy, DndAction::Copy);
        self.app.roundtrip();
        session.input(&format!("move {} {}", self.over_app.0 + 1, self.over_app.1 + 1));
        offer
    }

    /// Drop, read the data and finish, as the Wayland app.
    fn drop_here(&mut self, session: &mut HeadlessSession, offer: &wayland_client::protocol::wl_data_offer::WlDataOffer) {
        session.input(&format!("button {BTN_LEFT} 0"));
        self.app.until("the drag was never dropped on the Wayland window", |s| s.dropped);
        assert_eq!(self.app.receive(offer, "text/plain;charset=utf-8"), DATA);
        offer.finish();
        self.app.roundtrip();
    }
}

/// A point of `rect` (an X11 window) outside `others`, where the pointer
/// isn't over `app`'s windows.
fn point_over_x(session: &mut HeadlessSession, app: &mut common::Client, rect: (i32, i32, i32, i32), others: &[(i32, i32, i32, i32)]) -> (i32, i32) {
    let (x, y, w, h) = rect;
    (x + 10..x + w - 10)
        .step_by(10)
        .flat_map(|px| (y + 10..y + h - 10).step_by(10).map(move |py| (px, py)))
        .filter(|p| others.iter().all(|o| !common::xdnd::inside(*o, *p)))
        .find(|(px, py)| {
            session.input(&format!("move {px} {py}"));
            app.roundtrip();
            app.state.pointer.is_none()
        })
        .expect("no point of the X11 window that the others don't cover")
}

/// Press on `surface` at `start` and drag `types` from it.
fn begin_wayland_drag(session: &mut HeadlessSession, app: &mut common::Client, surface: &wayland_client::protocol::wl_surface::WlSurface, start: (i32, i32), types: &[(&str, &[u8])]) {
    session.input(&format!("move {} {}", start.0, start.1));
    app.until("the pointer never came back to the Wayland window", |s| s.pointer.as_ref() == Some(surface));
    app.state.press = None;
    session.input(&format!("button {BTN_LEFT} 1"));
    app.until("the Wayland window never got the press", |s| s.press.is_some());
    let press = app.state.press.unwrap();
    app.drag_types(types, surface, press);
}

/// Wait for what an X11 target reports while the Wayland app serves.
fn served<T: std::fmt::Debug>(app: &mut common::Client, rx: &std::sync::mpsc::Receiver<T>, want: impl Fn(&T) -> bool, what: &str) -> T {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        app.roundtrip();
        match rx.try_recv() {
            Ok(seen) if want(&seen) => return seen,
            Ok(_) => {}
            Err(_) => {
                assert!(Instant::now() < deadline, "{what}");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// An InputOnly window of `x`'s app, never mapped.
fn helper_window(x: &X11) -> u32 {
    let window = x.conn.generate_id().unwrap();
    x.conn.create_window(0, window, x.root, 0, 0, 1, 1, 0, WindowClass::INPUT_ONLY, 0, &CreateWindowAux::new()).unwrap();
    x.conn.flush().unwrap();
    window
}

/// X107 (S45): Chromium sends its XDND messages from another window than
/// the one owning XdndSelection: status and finished go to the window
/// that sent them, the data is asked of the owner.
#[test]
fn a_drag_whose_messages_come_from_another_window() {
    let Some(mut session) = session() else { return };
    let (mut messenger, mut owner) = (0, 0);
    let mut d = x11_drag_to_wayland(&mut session, |s| {
        let x = X11::new(s, (200, 150), false);
        (messenger, owner) = (helper_window(&x), x.window);
        (x, Source { messenger: Some(messenger), ..Source::default() })
    });
    let offer = d.accept(&mut session);
    expect(&d.saw, |s| *s == SourceSaw::Accepted(messenger), "XdndStatus never came to the window that sent the drag's messages");
    d.drop_here(&mut session, &offer);
    expect(&d.saw, |s| *s == SourceSaw::Sent(owner), "the data was never asked of the XdndSelection owner");
    expect(&d.saw, |s| matches!(s, SourceSaw::Finished(w, _) if *w == messenger), "XdndFinished never came to the window that sent the drag's messages");
}

/// X108, X109 (S46, S47): over a Wayland window the X11 app drags to the
/// window manager's drop window, which Chromium only takes for a target
/// with a name; its XdndFinished names it, says the drop was taken, and
/// the action.
#[test]
fn the_drop_window_over_wayland_is_named_and_finishes_in_its_own_name() {
    let Some(mut session) = session() else { return };
    let mut source_window = 0;
    let mut d = x11_drag_to_wayland(&mut session, |s| {
        let x = X11::new(s, (200, 150), false);
        source_window = x.window;
        (x, Source { named_only: true, ..Source::default() })
    });
    let probe = X11::unmapped(&session, (1, 1), false);
    assert!(probe.get32(d.target, probe.atoms.aware).is_some_and(|v| v >= XDND_VERSION), "the drop window {:#x} isn't XDND-aware of version 5", d.target);
    assert!(!probe.name_of(d.target).is_empty(), "the drop window has no WM_NAME (Chromium skips it)");
    let offer = d.accept(&mut session);
    d.drop_here(&mut session, &offer);
    let SourceSaw::Finished(to, data) = expect(&d.saw, |s| matches!(s, SourceSaw::Finished(..)), "the X11 source never heard the drop finished") else { unreachable!() };
    assert_eq!(to, source_window);
    assert_eq!(data[0], d.target, "XdndFinished names another window than the drop window");
    assert_eq!(data[1] & 1, 1, "XdndFinished says the drop wasn't taken");
    assert_eq!(data[2], probe.atoms.copy, "XdndFinished gives action {}", data[2]);
}

/// A Wayland app's window (made after the X11 windows, so over them),
/// and where to start a drag on it.
fn wayland_window_by(session: &mut HeadlessSession) -> (common::Client, wayland_client::protocol::wl_surface::WlSurface, (i32, i32)) {
    let mut app = common::connect(session);
    let (window, ()) = app.toplevel((200, 150), |_, _| {});
    let surface = window.surface.clone();
    std::mem::forget(window);
    let start = common::point_over(session, &mut app, &surface, (800, 600));
    (app, surface, start)
}

/// X110 (S48): a drop target with `XdndProxy` gets the drag through its
/// proxy window (which names itself as its proxy), in messages naming the
/// target; a proxy that doesn't name itself isn't followed.
#[test]
fn a_drop_target_behind_an_xdnd_proxy_gets_the_drag_there() {
    xdnd_proxy(true);
}

#[test]
fn a_broken_xdnd_proxy_is_not_followed() {
    xdnd_proxy(false);
}

fn xdnd_proxy(proper: bool) {
    let Some(mut session) = session() else { return };
    let target = X11::new(&session, (300, 200), true);
    let x_rect = target.rect();
    // The proxy: another app's (connection's) window, never mapped.
    let proxy = X11::unmapped(&session, (1, 1), true);
    let (t, p) = (target.window, proxy.window);
    target.conn.change_property32(PropMode::REPLACE, t, target.atoms.proxy, AtomEnum::WINDOW, &[p]).unwrap();
    target.conn.flush().unwrap();
    if proper {
        proxy.conn.change_property32(PropMode::REPLACE, p, proxy.atoms.proxy, AtomEnum::WINDOW, &[p]).unwrap();
        proxy.conn.flush().unwrap();
    }
    let (mut app, surface, start) = wayland_window_by(&mut session);
    let over_x = point_over_x(&mut session, &mut app, x_rect, &[]);
    let at_target = xdnd_target_app(target);
    let at_proxy = xdnd_target_app(proxy);

    begin_wayland_drag(&mut session, &mut app, &surface, start, &[("text/plain;charset=utf-8", DATA)]);
    glide(&mut session, start, over_x);
    let (gets, other, which) = if proper { (&at_proxy, &at_target, "proxy") } else { (&at_target, &at_proxy, "target itself") };
    let entered = served(&mut app, gets, |s| matches!(s, TargetSaw::Entered(..)), &format!("the {which} never got XdndEnter"));
    assert!(matches!(entered, TargetSaw::Entered(w, _) if w == t), "XdndEnter names {entered:?}, not the target {t:#x}");
    session.input(&format!("move {} {}", over_x.0 + 1, over_x.1 + 1));
    std::thread::sleep(Duration::from_millis(200));
    session.input(&format!("button {BTN_LEFT} 0"));
    let got = served(&mut app, gets, |s| matches!(s, TargetSaw::Dropped(_)), &format!("the {which} was never dropped on"));
    assert_eq!(got, TargetSaw::Dropped(DATA.to_vec()));
    assert!(other.try_recv().is_err(), "the other window got XDND messages too");
}

/// X111 (S49): the X11 drag source's window destroyed mid-drag (the app
/// crashed or closed it): the drag ends, and the pointer works again.
#[test]
fn a_drag_whose_source_goes_away_ends() {
    let Some(mut session) = session() else { return };
    let mut source = None;
    let mut d = x11_drag_to_wayland(&mut session, |s| {
        let x = X11::new(s, (200, 150), false);
        source = Some((x.conn.clone(), x.window));
        (x, Source::default())
    });
    let (conn, window) = source.unwrap();
    conn.destroy_window(window).unwrap();
    conn.flush().unwrap();
    d.app.until("the drag never ended after its X11 source went away", |s| s.dnd.is_none());
    session.input(&format!("button {BTN_LEFT} 0"));
    std::thread::sleep(Duration::from_millis(100));
    d.app.state.press = None;
    session.input(&format!("move {} {}", d.over_app.0, d.over_app.1));
    session.input(&format!("button {BTN_LEFT} 1"));
    d.app.until("the Wayland window never got a press again after the drag", |s| s.press.is_some());
    session.input(&format!("button {BTN_LEFT} 0"));
    assert!(d.app.state.pointer.as_ref() == Some(&d.surface), "the pointer isn't over the Wayland window");
}

/// X112 (S50): Chromium's drag icon is an override-redirect window kept
/// under the pointer: it isn't taken for the target, and menus take
/// clicks again after the drop.
#[test]
fn a_drag_icon_under_the_pointer_is_not_the_target() {
    let Some(mut session) = session() else { return };
    let mut d = x11_drag_to_wayland(&mut session, |s| {
        let x = X11::new(s, (200, 150), false);
        let icon = x.conn.generate_id().unwrap();
        let aux = CreateWindowAux::new().override_redirect(1).background_pixel(0);
        x.conn.create_window(0, icon, x.root, 0, 0, 16, 16, 0, WindowClass::INPUT_OUTPUT, 0, &aux).unwrap();
        x.conn.flush().unwrap();
        (x, Source { icon: Some(icon), ..Source::default() })
    });
    let offer = d.accept(&mut session);
    expect(&d.saw, |s| matches!(s, SourceSaw::Accepted(_)), "the X11 source never heard the Wayland window accept");
    d.drop_here(&mut session, &offer);
    expect(&d.saw, |s| matches!(s, SourceSaw::Finished(..)), "the X11 source never heard the drop finished");

    // A menu now, under the pointer: it gets the click.
    let menu_app = X11::unmapped(&session, (1, 1), false);
    let menu = menu_app.conn.generate_id().unwrap();
    let (mx, my) = (d.over_app.0 - 30, d.over_app.1 - 30);
    let aux = CreateWindowAux::new().override_redirect(1).background_pixel(0).event_mask(EventMask::BUTTON_PRESS | EventMask::STRUCTURE_NOTIFY);
    menu_app.conn.create_window(0, menu, menu_app.root, mx as i16, my as i16, 60, 60, 0, WindowClass::INPUT_OUTPUT, 0, &aux).unwrap();
    menu_app.conn.map_window(menu).unwrap();
    menu_app.conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(300));
    session.input(&format!("move {} {}", mx + 30, my + 30));
    std::thread::sleep(Duration::from_millis(100));
    session.input(&format!("button {BTN_LEFT} 1"));
    session.input(&format!("button {BTN_LEFT} 0"));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match menu_app.conn.poll_for_event().unwrap() {
            Some(Event::ButtonPress(p)) if p.event == menu => break,
            Some(_) => {}
            None => {
                assert!(Instant::now() < deadline, "the menu never got the click after the drag");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }
}

/// X113 (S51): a Wayland drag over one X11 window, back over a Wayland
/// window, then over another X11 window: the first is left, the drag
/// goes on, and the drop lands on the second.
#[test]
fn a_wayland_drag_goes_from_one_x11_window_to_another() {
    let Some(mut session) = session() else { return };
    // Crossed, so each has a part the other doesn't cover.
    let a = X11::new(&session, (300, 80), true);
    let b = X11::new(&session, (80, 300), true);
    let (a_rect, b_rect) = (a.rect(), b.rect());
    let (mut app, surface, start) = wayland_window_by(&mut session);
    let over_a = point_over_x(&mut session, &mut app, a_rect, &[b_rect]);
    let over_b = point_over_x(&mut session, &mut app, b_rect, &[a_rect]);
    let at_a = xdnd_target_app(a);
    let at_b = xdnd_target_app(b);

    begin_wayland_drag(&mut session, &mut app, &surface, start, &[("text/plain;charset=utf-8", DATA)]);
    glide(&mut session, start, over_a);
    served(&mut app, &at_a, |s| matches!(s, TargetSaw::Entered(..)), "the first X11 window never got XdndEnter");
    glide(&mut session, over_a, start);
    served(&mut app, &at_a, |s| *s == TargetSaw::Left, "the first X11 window never got XdndLeave");
    app.until("the drag never came back over the Wayland window", |s| s.dnd.is_some());
    assert_eq!(app.state.source_ended, None, "the Wayland drag ended when it left the X11 window");
    glide(&mut session, start, over_b);
    served(&mut app, &at_b, |s| matches!(s, TargetSaw::Entered(..)), "the second X11 window never got XdndEnter");
    session.input(&format!("move {} {}", over_b.0 + 1, over_b.1 + 1));
    std::thread::sleep(Duration::from_millis(200));
    session.input(&format!("button {BTN_LEFT} 0"));
    let got = served(&mut app, &at_b, |s| matches!(s, TargetSaw::Dropped(_)), "the second X11 window was never dropped on");
    assert_eq!(got, TargetSaw::Dropped(DATA.to_vec()));
}

/// A Wayland app dragging onto an X11 drop target that behaves as `how`
/// says, dropped: the target's reports, its app's connection and window,
/// and the Wayland app.
/// A drop on an X11 window: what its target saw, its app's connection,
/// its window, and the Wayland app.
type Dropped = (std::sync::mpsc::Receiver<TargetSaw>, std::sync::Arc<x11rb::rust_connection::RustConnection>, u32, common::Client);

fn wayland_drop_on_x11(session: &mut HeadlessSession, how: Target, types: &[(&str, &[u8])]) -> Dropped {
    let x = X11::new(session, (300, 200), true);
    let x_rect = x.rect();
    let (conn, window) = (x.conn.clone(), x.window);
    let (mut app, surface, start) = wayland_window_by(session);
    let over_x = point_over_x(session, &mut app, x_rect, &[]);
    let saw = xdnd_target_with(x, how);
    begin_wayland_drag(session, &mut app, &surface, start, types);
    glide(session, start, over_x);
    served(&mut app, &saw, |s| matches!(s, TargetSaw::Entered(..)), "the X11 window never got XdndEnter");
    session.input(&format!("move {} {}", over_x.0 + 1, over_x.1 + 1));
    std::thread::sleep(Duration::from_millis(200));
    session.input(&format!("button {BTN_LEFT} 0"));
    (saw, conn, window, app)
}

/// X115 (W77): an X11 target that takes nothing from the drop ends the
/// drag with XdndFinished (performed false); one from another window
/// before it is ignored, and the source stays until the real one.
#[test]
fn a_drop_the_target_refuses_ends_the_drag_and_a_forged_finish_does_not() {
    let Some(mut session) = session() else { return };
    let how = Target { leave_to_test: true, ..Target::default() };
    let (saw, conn, window, mut app) = wayland_drop_on_x11(&mut session, how, &[("text/plain;charset=utf-8", DATA)]);
    let TargetSaw::Drop(source, _) = served(&mut app, &saw, |s| matches!(s, TargetSaw::Drop(..)), "the X11 window never got XdndDrop") else { unreachable!() };

    let forger = X11::unmapped(&session, (1, 1), false);
    message(&forger.conn, source, forger.atoms.finished, [forger.window, 1, forger.atoms.copy, 0, 0]);
    let until = Instant::now() + Duration::from_millis(500);
    while Instant::now() < until {
        app.roundtrip();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(app.state.source_ended, None, "the Wayland drag ended before its X11 target finished (on a forged XdndFinished?)");

    let atoms = forger.atoms;
    message_as(&conn, source, source, atoms.finished, [window, 0, x11rb::NONE, 0, 0]);
    app.until("the Wayland drag never ended after the X11 target finished without taking it", |s| s.source_ended.is_some());
}

/// X116 (W78): an X11 target that asks for the data a while after the
/// drop still gets it.
#[test]
fn a_drop_fetched_late_is_still_served() {
    let Some(mut session) = session() else { return };
    let how = Target { fetch_after: Duration::from_millis(400), ..Target::default() };
    let (saw, _conn, _window, mut app) = wayland_drop_on_x11(&mut session, how, &[("text/plain;charset=utf-8", DATA)]);
    let got = served(&mut app, &saw, |s| matches!(s, TargetSaw::Dropped(_)), "the X11 window never got the data it asked for late");
    assert_eq!(got, TargetSaw::Dropped(DATA.to_vec()));
}

/// X118 (W80): Chromium asks for several types at once during a drop:
/// each is answered with its own data.
#[test]
fn several_types_fetched_at_once() {
    several_types(false);
}

/// The same, one after the other (how most toolkits ask).
#[test]
fn several_types_fetched_one_after_another() {
    several_types(true);
}

fn several_types(one_by_one: bool) {
    let Some(mut session) = session() else { return };
    let types: [(&str, &[u8]); 3] = [("text/html", b"<b>bold</b>"), ("image/png", b"not really a png"), ("application/x-helix-test", b"the third")];
    let probe = X11::unmapped(&session, (1, 1), false);
    let how = Target { fetch_all: true, one_by_one, ..Target::default() };
    let (saw, _conn, _window, mut app) = wayland_drop_on_x11(&mut session, how, &types);
    let TargetSaw::DroppedAll(got) = served(&mut app, &saw, |s| matches!(s, TargetSaw::DroppedAll(_)), "the X11 window never got all the types it asked for") else { unreachable!() };
    let got: Vec<(String, Vec<u8>)> = got.into_iter().map(|(k, d)| (probe.atom_name(k), d)).collect();
    for (mime, data) in types {
        let found = got.iter().find(|(name, _)| name == mime);
        assert_eq!(found.map(|(_, d)| d.as_slice()), Some(data), "{mime}: got {got:?}");
    }
}

/// X117 (W79): the X11 target window destroyed mid-drag: the drag goes
/// on and can be dropped elsewhere.
#[test]
fn a_drag_whose_x11_target_goes_away_goes_on() {
    let Some(mut session) = session() else { return };
    let x = X11::new(&session, (300, 200), true);
    let x_rect = x.rect();
    let (conn, window) = (x.conn.clone(), x.window);
    let (mut app, surface, start) = wayland_window_by(&mut session);
    let over_x = point_over_x(&mut session, &mut app, x_rect, &[]);
    let saw = xdnd_target_app(x);
    begin_wayland_drag(&mut session, &mut app, &surface, start, &[("text/plain;charset=utf-8", DATA)]);
    glide(&mut session, start, over_x);
    served(&mut app, &saw, |s| matches!(s, TargetSaw::Entered(..)), "the X11 window never got XdndEnter");
    app.roundtrip();
    assert!(app.state.dnd.is_none(), "the drag is still over the Wayland window");

    conn.destroy_window(window).unwrap();
    conn.flush().unwrap();
    std::thread::sleep(Duration::from_millis(200));
    glide(&mut session, over_x, start);
    app.until("the drag never came back over the Wayland window after the X11 target went away", |s| s.dnd.is_some());
    let (offer, _, serial) = app.state.dnd.clone().unwrap();
    offer.accept(serial, Some("text/plain;charset=utf-8".to_owned()));
    offer.set_actions(DndAction::Copy, DndAction::Copy);
    app.roundtrip();
    session.input(&format!("move {} {}", start.0 + 1, start.1 + 1));
    session.input(&format!("button {BTN_LEFT} 0"));
    app.until("the drag was never dropped on the Wayland window", |s| s.dropped);
    assert_eq!(app.receive(&offer, "text/plain;charset=utf-8"), DATA);
    // And the window manager still manages new windows.
    let _again = X11::new(&session, (100, 100), false);
}
