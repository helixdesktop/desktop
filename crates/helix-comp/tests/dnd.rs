//! Drag and drop between two Wayland apps: one drags (`start_drag` for
//! its button press), the other is offered the drag, accepts it and gets
//! the data when it is dropped. Since Smithay's drag-and-drop rework the
//! compositor starts the drag's grab itself (`WaylandDndGrabHandler`).
//!
//! Runs the real `helix-comp` headless; needs `dbus-daemon` (as every
//! headless session), skipped otherwise.

mod common;

use std::time::Duration;

use helix_test_support::headless::{HeadlessSession, Options};
use wayland_client::protocol::wl_data_device_manager::DndAction;

const BTN_LEFT: u32 = 0x110;
const MIME: &str = "text/plain;charset=utf-8";
const DATA: &[u8] = b"dragged between Wayland apps";

#[test]
fn a_wayland_app_drags_onto_another_wayland_window() {
    let comp = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-comp"));
    let Some(mut session) = HeadlessSession::start(Options { comp: &comp, shell: None, size: (800, 600), settings: "", env: Vec::new() }) else { return };

    // The target first and larger, so the source (on top, and raised by
    // the press anyway) leaves part of it uncovered.
    let mut target = common::connect(&session);
    let (to, ()) = target.toplevel((400, 300), |_, _| {});
    let to_surface = to.surface.clone();
    let mut source = common::connect(&session);
    let (from, ()) = source.toplevel((150, 100), |_, _| {});
    let from_surface = from.surface.clone();
    let over_target = common::point_over(&mut session, &mut target, &to_surface, (800, 600));
    let start = common::point_over(&mut session, &mut source, &from_surface, (800, 600));

    session.input(&format!("button {BTN_LEFT} 1"));
    source.until("the source window never got the press", |s| s.press.is_some());
    let press = source.state.press.unwrap();
    source.drag(MIME, DATA, &from_surface, press);
    for i in 1..=8 {
        let x = start.0 + (over_target.0 - start.0) * i / 8;
        let y = start.1 + (over_target.1 - start.1) * i / 8;
        session.input(&format!("move {x} {y}"));
        std::thread::sleep(Duration::from_millis(20));
    }

    target.until("the target window was never offered the drag", |s| s.dnd.is_some());
    let (offer, types, serial) = target.state.dnd.clone().unwrap();
    assert!(types.iter().any(|t| t == MIME), "offered {types:?}");
    offer.accept(serial, Some(MIME.to_owned()));
    offer.set_actions(DndAction::Copy, DndAction::Copy);
    target.roundtrip();
    source.roundtrip();
    session.input(&format!("move {} {}", over_target.0 + 1, over_target.1 + 1));
    target.roundtrip();
    source.roundtrip();

    session.input(&format!("button {BTN_LEFT} 0"));
    target.until("the drag was never dropped on the target window", |s| s.dropped);
    // The source serves the data from its own queue: read on a thread
    // while it is dispatched here.
    let reader = std::thread::spawn({
        let offer = offer.clone();
        move || {
            let mut target = target;
            let data = target.receive(&offer, MIME);
            offer.finish();
            target.roundtrip();
            data
        }
    });
    while !reader.is_finished() {
        source.roundtrip();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(reader.join().unwrap(), DATA);
}
