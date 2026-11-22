//! A Wayland client for headless focus tests: toplevels with plain
//! buffers, the keyboard and pointer focus it is told about, the serial
//! of the last key or button press it got, activation tokens, and the
//! clipboard selection it is offered or sets.

#![allow(dead_code)]

pub mod xdnd;
pub mod xwm;

use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use helix_protocols::gtk_shell_client::{gtk_shell1, gtk_surface1};
use helix_test_support::headless::HeadlessSession;
use wayland_client::globals::{registry_queue_init, GlobalListContents};
use wayland_client::protocol::{wl_buffer, wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer, wl_data_source, wl_keyboard, wl_pointer, wl_registry, wl_seat, wl_shm, wl_shm_pool, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::wp::primary_selection::zv1::client::{zwp_primary_selection_device_manager_v1 as primary_manager, zwp_primary_selection_device_v1 as primary_device, zwp_primary_selection_offer_v1 as primary_offer};
use wayland_protocols::xdg::activation::v1::client::{xdg_activation_token_v1, xdg_activation_v1};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

#[derive(Default)]
pub struct State {
    /// The surface with keyboard focus.
    pub keyboard: Option<wl_surface::WlSurface>,
    /// The surface under the pointer.
    pub pointer: Option<wl_surface::WlSurface>,
    /// Pending configure serials, per xdg_surface.
    configured: Vec<(xdg_surface::XdgSurface, u32)>,
    token: Option<String>,
    /// The serial of the last key or button press this client got.
    pub press: Option<u32>,
    /// Whether the last toplevel configure said maximized.
    pub maximized: bool,
    /// The clipboard selection offered (while focused) and its types.
    pub selection: Option<(wl_data_offer::WlDataOffer, Vec<String>)>,
    /// Types of offers announced but not yet made the selection.
    offers: Vec<(wl_data_offer::WlDataOffer, Vec<String>)>,
    /// What this client's own selection or drag (`Client::copy`,
    /// `Client::drag`) holds: by type, and for any other type asked for.
    copied: Vec<u8>,
    copied_as: Vec<(String, Vec<u8>)>,
    /// How this client's last drag ended for its source: `"cancelled"`,
    /// `"dnd_finished"` (or still `None`); and whether it was dropped
    /// (`dnd_drop_performed`).
    pub source_ended: Option<&'static str>,
    pub source_dropped: bool,
    /// The primary selection offered (while focused) and its types.
    pub primary: Option<(primary_offer::ZwpPrimarySelectionOfferV1, Vec<String>)>,
    primary_offers: Vec<(primary_offer::ZwpPrimarySelectionOfferV1, Vec<String>)>,
    /// A drag over one of this client's surfaces: its offer, types and
    /// the enter serial.
    pub dnd: Option<(wl_data_offer::WlDataOffer, Vec<String>, u32)>,
    /// Whether that drag was dropped here.
    pub dropped: bool,
}

macro_rules! quiet {
    ($($iface:ty),*) => {$(
        impl Dispatch<$iface, ()> for State {
            fn event(_: &mut Self, _: &$iface, _: <$iface as Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
        }
    )*};
}
quiet!(wl_data_device_manager::WlDataDeviceManager, wl_compositor::WlCompositor, wl_surface::WlSurface, wl_shm::WlShm, wl_shm_pool::WlShmPool, wl_buffer::WlBuffer, wl_seat::WlSeat, xdg_activation_v1::XdgActivationV1, gtk_shell1::GtkShell1, gtk_surface1::GtkSurface1);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn event(_: &mut Self, base: &xdg_wm_base::XdgWmBase, event: xdg_wm_base::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            base.pong(serial);
        }
    }
}
impl Dispatch<xdg_toplevel::XdgToplevel, ()> for State {
    fn event(state: &mut Self, _: &xdg_toplevel::XdgToplevel, event: xdg_toplevel::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_toplevel::Event::Configure { states, .. } = event {
            let maximized = xdg_toplevel::State::Maximized as u32;
            state.maximized = states.as_chunks::<4>().0.iter().any(|c| u32::from_ne_bytes(*c) == maximized);
        }
    }
}
impl Dispatch<xdg_surface::XdgSurface, ()> for State {
    fn event(state: &mut Self, surface: &xdg_surface::XdgSurface, event: xdg_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_surface::Event::Configure { serial } = event {
            state.configured.push((surface.clone(), serial));
        }
    }
}
impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(state: &mut Self, _: &wl_keyboard::WlKeyboard, event: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_keyboard::Event::Enter { surface, .. } => state.keyboard = Some(surface),
            wl_keyboard::Event::Leave { .. } => state.keyboard = None,
            wl_keyboard::Event::Key { serial, state: wayland_client::WEnum::Value(wl_keyboard::KeyState::Pressed), .. } => state.press = Some(serial),
            _ => {}
        }
    }
}
impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(state: &mut Self, _: &wl_pointer::WlPointer, event: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_pointer::Event::Enter { surface, .. } => state.pointer = Some(surface),
            wl_pointer::Event::Leave { .. } => state.pointer = None,
            wl_pointer::Event::Button { serial, state: wayland_client::WEnum::Value(wl_pointer::ButtonState::Pressed), .. } => state.press = Some(serial),
            _ => {}
        }
    }
}
impl Dispatch<xdg_activation_token_v1::XdgActivationTokenV1, ()> for State {
    fn event(state: &mut Self, _: &xdg_activation_token_v1::XdgActivationTokenV1, event: xdg_activation_token_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_activation_token_v1::Event::Done { token } = event {
            state.token = Some(token);
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
    fn event(state: &mut Self, _: &wl_data_device::WlDataDevice, event: wl_data_device::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_data_device::Event::DataOffer { id } => state.offers.push((id, Vec::new())),
            wl_data_device::Event::Selection { id } => {
                state.selection = id.map(|id| {
                    let types = state.offers.iter().find(|(o, _)| o == &id).map(|(_, t)| t.clone()).unwrap_or_default();
                    (id, types)
                });
                state.offers.clear();
            }
            wl_data_device::Event::Enter { serial, id, .. } => {
                state.dnd = id.map(|id| {
                    let types = state.offers.iter().find(|(o, _)| o == &id).map(|(_, t)| t.clone()).unwrap_or_default();
                    (id, types, serial)
                });
                state.dropped = false;
            }
            wl_data_device::Event::Leave => state.dnd = None,
            wl_data_device::Event::Drop => state.dropped = true,
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
    ]);
}
impl Dispatch<wl_data_offer::WlDataOffer, ()> for State {
    fn event(state: &mut Self, offer: &wl_data_offer::WlDataOffer, event: wl_data_offer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_data_offer::Event::Offer { mime_type } = event {
            if let Some((_, types)) = state.offers.iter_mut().find(|(o, _)| o == offer) {
                types.push(mime_type);
            }
        }
    }
}

impl Dispatch<wl_data_source::WlDataSource, ()> for State {
    fn event(state: &mut Self, _: &wl_data_source::WlDataSource, event: wl_data_source::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            wl_data_source::Event::Send { fd, mime_type } => {
                let data = state.copied_as.iter().find(|(m, _)| *m == mime_type).map_or_else(|| state.copied.clone(), |(_, d)| d.clone());
                std::thread::spawn(move || {
                    let _ = std::io::Write::write_all(&mut std::fs::File::from(fd), &data);
                });
            }
            wl_data_source::Event::Cancelled => state.source_ended = Some("cancelled"),
            wl_data_source::Event::DndFinished => state.source_ended = Some("dnd_finished"),
            wl_data_source::Event::DndDropPerformed => state.source_dropped = true,
            _ => {}
        }
    }
}

quiet!(primary_manager::ZwpPrimarySelectionDeviceManagerV1);
impl Dispatch<primary_device::ZwpPrimarySelectionDeviceV1, ()> for State {
    fn event(state: &mut Self, _: &primary_device::ZwpPrimarySelectionDeviceV1, event: primary_device::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {
            primary_device::Event::DataOffer { offer } => state.primary_offers.push((offer, Vec::new())),
            primary_device::Event::Selection { id } => {
                state.primary = id.map(|id| {
                    let types = state.primary_offers.iter().find(|(o, _)| o == &id).map(|(_, t)| t.clone()).unwrap_or_default();
                    (id, types)
                });
                state.primary_offers.clear();
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, primary_device::ZwpPrimarySelectionDeviceV1, [
        primary_device::EVT_DATA_OFFER_OPCODE => (primary_offer::ZwpPrimarySelectionOfferV1, ()),
    ]);
}
impl Dispatch<primary_offer::ZwpPrimarySelectionOfferV1, ()> for State {
    fn event(state: &mut Self, offer: &primary_offer::ZwpPrimarySelectionOfferV1, event: primary_offer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let primary_offer::Event::Offer { mime_type } = event {
            if let Some((_, types)) = state.primary_offers.iter_mut().find(|(o, _)| o == offer) {
                types.push(mime_type);
            }
        }
    }
}

fn filled(shm: &wl_shm::WlShm, qh: &QueueHandle<State>, (w, h): (i32, i32)) -> wl_buffer::WlBuffer {
    let bytes: Vec<u8> = std::iter::repeat_n(0xFF80_8080u32.to_le_bytes(), (w * h) as usize).flatten().collect();
    // SAFETY: memfd_create returns a new fd we own (or -1).
    let raw = unsafe { libc::memfd_create(c"modal-test".as_ptr(), libc::MFD_CLOEXEC) };
    assert!(raw >= 0);
    // SAFETY: just created.
    let fd = unsafe { <std::os::fd::OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) };
    std::io::Write::write_all(&mut std::fs::File::from(fd.try_clone().unwrap()), &bytes).unwrap();
    let pool = shm.create_pool(fd.as_fd(), bytes.len() as i32, qh, ());
    let buffer = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, ());
    pool.destroy();
    buffer
}

pub struct Client {
    queue: EventQueue<State>,
    pub qh: QueueHandle<State>,
    pub state: State,
    compositor: wl_compositor::WlCompositor,
    shm: wl_shm::WlShm,
    base: xdg_wm_base::XdgWmBase,
    activation: xdg_activation_v1::XdgActivationV1,
    pub gtk_shell: gtk_shell1::GtkShell1,
    pub seat: wl_seat::WlSeat,
    pub data_device: wl_data_device::WlDataDevice,
    data_manager: wl_data_device_manager::WlDataDeviceManager,
    /// The primary selection device, when the compositor offers one.
    pub primary_device: Option<primary_device::ZwpPrimarySelectionDeviceV1>,
}

pub struct Toplevel {
    pub surface: wl_surface::WlSurface,
    pub xdg: xdg_surface::XdgSurface,
    pub toplevel: xdg_toplevel::XdgToplevel,
}

impl Client {
    pub fn roundtrip(&mut self) {
        self.queue.roundtrip(&mut self.state).unwrap();
    }

    /// Run until `done` holds (or fail with `what`).
    pub fn until(&mut self, what: &str, done: impl Fn(&State) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            self.roundtrip();
            if done(&self.state) {
                return;
            }
            assert!(Instant::now() < deadline, "{what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// A mapped toplevel of `size`; `before_commit` runs before its
    /// initial commit.
    pub fn toplevel<R>(&mut self, size: (i32, i32), before_commit: impl FnOnce(&Self, &Toplevel) -> R) -> (Toplevel, R) {
        let surface = self.compositor.create_surface(&self.qh, ());
        let xdg = self.base.get_xdg_surface(&surface, &self.qh, ());
        let toplevel = xdg.get_toplevel(&self.qh, ());
        let t = Toplevel { surface, xdg, toplevel };
        let made = before_commit(self, &t);
        t.surface.commit();
        let xdg = t.xdg.clone();
        self.until("no first configure", |s| s.configured.iter().any(|(x, _)| x == &xdg));
        let serial = self.state.configured.iter().rev().find(|(x, _)| x == &xdg).unwrap().1;
        self.state.configured.retain(|(x, _)| x != &xdg);
        t.xdg.ack_configure(serial);
        t.surface.attach(Some(&filled(&self.shm, &self.qh, size)), 0, 0);
        t.surface.damage_buffer(0, 0, size.0, size.1);
        t.surface.commit();
        self.roundtrip();
        (t, made)
    }

    /// A token asked for from `from` (none: no surface) with `serial` on
    /// the seat (none: no serial).
    pub fn token(&mut self, from: Option<&wl_surface::WlSurface>, serial: Option<u32>) -> String {
        self.state.token = None;
        let token = self.activation.get_activation_token(&self.qh, ());
        if let Some(surface) = from {
            token.set_surface(surface);
        }
        if let Some(serial) = serial {
            token.set_serial(serial, &self.seat);
        }
        token.commit();
        self.until("no activation token", |s| s.token.is_some());
        token.destroy();
        self.state.token.clone().unwrap()
    }

    /// Activate `surface` with `token`.
    pub fn activate_token(&mut self, token: String, surface: &wl_surface::WlSurface) {
        self.activation.activate(token, surface);
        self.roundtrip();
        self.roundtrip();
    }

    /// Activate `surface` with a token for the last press this client
    /// got, as Chromium does for its own window after a key press.
    pub fn activate(&mut self, surface: &wl_surface::WlSurface) {
        self.roundtrip();
        let press = self.state.press;
        let token = self.token(Some(surface), press);
        self.activate_token(token, surface);
    }

    pub fn focused(&mut self) -> Option<wl_surface::WlSurface> {
        self.roundtrip();
        self.state.keyboard.clone()
    }

    /// Set the clipboard to `data` as `mime` (the client must have
    /// keyboard focus).
    pub fn copy(&mut self, mime: &str, data: &[u8]) {
        self.state.copied = data.to_vec();
        self.state.copied_as.clear();
        let source = self.data_manager.create_data_source(&self.qh, ());
        source.offer(mime.to_owned());
        self.data_device.set_selection(Some(&source), 0);
        self.roundtrip();
    }

    /// Start dragging `data` as `mime` from `origin`, for the press of
    /// `serial` (offered for copying).
    pub fn drag(&mut self, mime: &str, data: &[u8], origin: &wl_surface::WlSurface, serial: u32) {
        self.drag_types(&[(mime, data)], origin, serial);
    }

    /// Start dragging `types` (each type with its own data) from
    /// `origin`, for the press of `serial` (offered for copying).
    pub fn drag_types(&mut self, types: &[(&str, &[u8])], origin: &wl_surface::WlSurface, serial: u32) {
        self.state.copied = types.first().map(|(_, d)| d.to_vec()).unwrap_or_default();
        self.state.copied_as = types.iter().map(|(m, d)| ((*m).to_owned(), d.to_vec())).collect();
        self.state.source_ended = None;
        self.state.source_dropped = false;
        let source = self.data_manager.create_data_source(&self.qh, ());
        for (mime, _) in types {
            source.offer((*mime).to_owned());
        }
        source.set_actions(wl_data_device_manager::DndAction::Copy);
        self.data_device.start_drag(Some(&source), origin, None, serial);
        self.roundtrip();
    }

    /// Read `offer`'s data as `mime` to the end.
    pub fn receive(&mut self, offer: &wl_data_offer::WlDataOffer, mime: &str) -> Vec<u8> {
        let (read, write) = std::io::pipe().unwrap();
        offer.receive(mime.to_owned(), write.as_fd());
        drop(write);
        self.roundtrip();
        let reader = std::thread::spawn(move || {
            let mut data = Vec::new();
            std::io::Read::read_to_end(&mut { read }, &mut data).map(|_| data)
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !reader.is_finished() {
            assert!(Instant::now() < deadline, "the dropped data never ended");
            self.roundtrip();
            std::thread::sleep(Duration::from_millis(10));
        }
        reader.join().unwrap().unwrap()
    }

    /// The clipboard selection's data as `mime`, read to the end.
    pub fn paste(&mut self, mime: &str) -> Vec<u8> {
        let (offer, _) = self.state.selection.clone().expect("no clipboard selection offered");
        let (read, write) = std::io::pipe().unwrap();
        offer.receive(mime.to_owned(), write.as_fd());
        drop(write);
        self.roundtrip();
        let reader = std::thread::spawn(move || {
            let mut data = Vec::new();
            std::io::Read::read_to_end(&mut { read }, &mut data).map(|_| data)
        });
        let deadline = Instant::now() + Duration::from_secs(10);
        while !reader.is_finished() {
            assert!(Instant::now() < deadline, "the clipboard data never ended");
            self.roundtrip();
            std::thread::sleep(Duration::from_millis(10));
        }
        reader.join().unwrap().unwrap()
    }
}


/// A client of `session`'s compositor with a keyboard and a pointer.
pub fn connect(session: &HeadlessSession) -> Client {
    let conn = Connection::from_socket(UnixStream::connect(&session.wayland_display).unwrap()).unwrap();
    let (globals, queue) = registry_queue_init::<State>(&conn).unwrap();
    let qh = queue.handle();
    let seat: wl_seat::WlSeat = globals.bind(&qh, 5..=9, ()).unwrap();
    seat.get_keyboard(&qh, ());
    seat.get_pointer(&qh, ());
    let manager: wl_data_device_manager::WlDataDeviceManager = globals.bind(&qh, 3..=3, ()).unwrap();
    let data_device = manager.get_data_device(&seat, &qh, ());
    let primary_device = globals.bind::<primary_manager::ZwpPrimarySelectionDeviceManagerV1, _, _>(&qh, 1..=1, ()).ok().map(|m| m.get_device(&seat, &qh, ()));
    Client {
        primary_device,
        compositor: globals.bind(&qh, 4..=6, ()).unwrap(),
        shm: globals.bind(&qh, 1..=1, ()).unwrap(),
        base: globals.bind(&qh, 1..=6, ()).unwrap(),
        activation: globals.bind(&qh, 1..=1, ()).unwrap(),
        gtk_shell: globals.bind(&qh, 1..=7, ()).expect("helix-comp offers gtk_shell1"),
        seat,
        data_device,
        data_manager: manager,
        queue,
        qh,
        state: State::default(),
    }
}

/// A point of the screen (`size`) where the pointer is over `surface` of
/// `c`: where windows go is Helix's choice, so look for it.
pub fn point_over(session: &mut HeadlessSession, c: &mut Client, surface: &wl_surface::WlSurface, (w, h): (i32, i32)) -> (i32, i32) {
    let over = (0..w).step_by(40).flat_map(|x| (0..h).step_by(40).map(move |y| (x, y))).find(|(x, y)| {
        session.input(&format!("move {x} {y}"));
        c.roundtrip();
        c.state.pointer.as_ref() == Some(surface)
    });
    over.expect("the pointer is never over the surface")
}

/// A connection to `session`'s Xwayland. x11rb reads its authority from
/// `XAUTHORITY`, which is the whole process's: tests running at once
/// (each with its own session) take turns.
pub fn x11(session: &HeadlessSession) -> (x11rb::rust_connection::RustConnection, usize) {
    static ENV: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _turn = ENV.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("XAUTHORITY", &session.x_authority);
    match x11rb::connect(Some(&session.x_display)) {
        Ok(c) => c,
        Err(x11rb::errors::ConnectError::SetupFailed(f)) => panic!("connect to Xwayland {}: refused: {}", session.x_display, String::from_utf8_lossy(&f.reason)),
        Err(err) => panic!("connect to Xwayland {}: {err:?}", session.x_display),
    }
}
