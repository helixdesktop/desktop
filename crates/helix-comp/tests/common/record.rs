//! What the window manager asks of the X server, as it asks it (the X
//! RECORD extension): every property it writes, with the value, and every
//! window it gives the X input focus to. An app sees each of these as it
//! happens (a PropertyNotify, a FocusIn), even when a later one undoes it
//! straight away, which is what reading the end result can't show (#288).

use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use helix_test_support::headless::HeadlessSession;
use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::record::{self, ConnectionExt as _};
use x11rb::protocol::xproto::Window;
use x11rb::rust_connection::RustConnection;

/// One request the recording saw, from a client other than the test's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// ChangeProperty: the window, the property's atom and its 32-bit
    /// values (other formats: empty).
    Property { window: Window, property: u32, values: Vec<u32> },
    /// SetInputFocus to `window`.
    Focus { window: Window },
}

/// A recording of the requests every other client makes, until dropped.
pub struct Recorder {
    ctrl: RustConnection,
    context: u32,
    seen: Arc<Mutex<Vec<Request>>>,
    thread: Option<JoinHandle<()>>,
}

const CHANGE_PROPERTY: u8 = 18;
const SET_INPUT_FOCUS: u8 = 42;
/// Reply categories (RECORD 1.13).
const FROM_CLIENT: u8 = 1;
const START_OF_DATA: u8 = 4;

impl Recorder {
    /// Start recording what clients other than `ignore` (one of the
    /// test's connections, by its resource id base) ask; returns once the
    /// recording is on.
    pub fn start(session: &HeadlessSession, ignore: &RustConnection) -> Self {
        let ignore = ignore.setup().resource_id_base;
        let (ctrl, _) = super::x11(session);
        let (data, _) = super::x11(session);
        assert!(ctrl.extension_information(record::X11_EXTENSION_NAME).unwrap().is_some(), "Xwayland has no RECORD extension");
        ctrl.record_query_version(1, 13).unwrap().reply().unwrap();
        let context = ctrl.generate_id().unwrap();
        let none = record::Range8 { first: 0, last: 0 };
        let no_ext = record::ExtRange { major: none, minor: record::Range16 { first: 0, last: 0 } };
        let requests = |op: u8| record::Range {
            core_requests: record::Range8 { first: op, last: op },
            core_replies: none,
            ext_requests: no_ext,
            ext_replies: no_ext,
            delivered_events: none,
            device_events: none,
            errors: none,
            client_started: false,
            client_died: false,
        };
        ctrl.record_create_context(context, 0, &[record::CS::ALL_CLIENTS.into()], &[requests(CHANGE_PROPERTY), requests(SET_INPUT_FOCUS)]).unwrap().check().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let (started_tx, started) = std::sync::mpsc::channel();
        let thread = {
            let seen = seen.clone();
            std::thread::spawn(move || {
                for reply in data.record_enable_context(context).unwrap() {
                    let Ok(reply) = reply else { return };
                    match reply.category {
                        START_OF_DATA => {
                            let _ = started_tx.send(());
                        }
                        FROM_CLIENT if reply.xid_base != ignore && !reply.client_swapped => {
                            seen.lock().unwrap().extend(parse(&reply.data));
                        }
                        _ => {}
                    }
                }
            })
        };
        started.recv_timeout(std::time::Duration::from_secs(5)).expect("the X recording never started");
        Recorder { ctrl, context, seen, thread: Some(thread) }
    }

    /// Everything recorded so far, in order.
    pub fn seen(&self) -> Vec<Request> {
        self.seen.lock().unwrap().clone()
    }

    /// The values `property` (an atom) was given on `window`, in order.
    pub fn values(&self, window: Window, property: u32) -> Vec<Vec<u32>> {
        self.seen()
            .into_iter()
            .filter_map(|r| match r {
                Request::Property { window: w, property: p, values } if w == window && p == property => Some(values),
                _ => None,
            })
            .collect()
    }

    /// Whether `window` was ever given the X input focus.
    pub fn focused(&self, window: Window) -> bool {
        self.seen().contains(&Request::Focus { window })
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        let _ = self.ctrl.record_disable_context(self.context);
        let _ = self.ctrl.record_free_context(self.context);
        let _ = self.ctrl.flush();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// The requests in recorded data (little-endian clients, no headers).
fn parse(mut data: &[u8]) -> Vec<Request> {
    let u32_at = |d: &[u8], at: usize| d.get(at..at + 4).map_or(0, |b| u32::from_le_bytes(b.try_into().unwrap()));
    let mut found = Vec::new();
    while data.len() >= 4 {
        let mut len = usize::from(u16::from_le_bytes([data[2], data[3]])) * 4;
        let mut body = 0;
        if len == 0 {
            // BIG-REQUESTS: the length follows.
            len = u32_at(data, 4) as usize * 4;
            body = 4;
        }
        if len < 4 || len > data.len() {
            break;
        }
        let request = &data[..len];
        match request[0] {
            CHANGE_PROPERTY => {
                let format = request.get(body + 16).copied().unwrap_or(0);
                let count = u32_at(request, body + 20) as usize;
                let values = if format == 32 { (0..count).map(|i| u32_at(request, body + 24 + 4 * i)).collect() } else { Vec::new() };
                found.push(Request::Property { window: u32_at(request, body + 4), property: u32_at(request, body + 8), values });
            }
            SET_INPUT_FOCUS => found.push(Request::Focus { window: u32_at(request, body + 4) }),
            _ => {}
        }
        data = &data[len..];
    }
    found
}
