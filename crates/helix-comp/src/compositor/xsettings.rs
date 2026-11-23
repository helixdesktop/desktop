//! X11 apps' scale and settings (helixdesktop/spec
//! `contracts/wayland/README.md`, "X11 apps: scaling and settings"):
//! Xwayland's coordinate space is scaled by Smithay's client scale, and
//! Helix is the XSETTINGS manager of Xwayland's screen (as
//! gnome-settings-daemon is in GNOME) and sets the X resources.

use helix_comp::xsettings::{self as xs, Settings};
use smithay::reexports::x11rb::connection::Connection;
use smithay::reexports::x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageEvent, ConnectionExt, CreateWindowAux, EventMask, PropMode, Window, WindowClass,
};
use smithay::reexports::x11rb::rust_connection::RustConnection;
use smithay::reexports::x11rb::CURRENT_TIME;
use smithay::xwayland::XWaylandClientData;

use super::Helix;

/// Helix's XSETTINGS manager window on Xwayland.
/// The XSETTINGS manager. Publishing needs an X reply (the root's
/// `RESOURCE_MANAGER`, merged with ours), so it is done on a thread of its
/// own (#249): this is an ordinary X client, and one holding the X server
/// (XGrabServer) would stop the whole desktop.
pub struct XSettings {
    serial: u32,
    published: Option<Vec<u8>>,
    jobs: std::sync::mpsc::Sender<Publish>,
}

/// What to publish: new XSETTINGS bytes (if they changed), and Helix's
/// X resources to merge into `RESOURCE_MANAGER`.
struct Publish {
    settings: Option<Vec<u8>>,
    resources: Vec<(&'static str, String)>,
}

fn x_err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

impl XSettings {
    /// Become the XSETTINGS manager of Xwayland `:display`.
    pub fn start(display: u32) -> Result<Self, String> {
        let (conn, screen) = helix_comp::xauth::connect(display)?;
        let root = conn.setup().roots[screen].root;
        let atom = |name: &str| -> Result<Atom, String> { Ok(conn.intern_atom(false, name.as_bytes()).map_err(x_err)?.reply().map_err(x_err)?.atom) };
        let selection = atom(&format!("_XSETTINGS_S{screen}"))?;
        let settings_atom = atom("_XSETTINGS_SETTINGS")?;
        let manager = atom("MANAGER")?;
        let resource_manager = atom("RESOURCE_MANAGER")?;
        let window = conn.generate_id().map_err(x_err)?;
        conn.create_window(0, window, root, -1, -1, 1, 1, 0, WindowClass::INPUT_ONLY, 0, &CreateWindowAux::new().override_redirect(1))
            .map_err(x_err)?
            .check()
            .map_err(x_err)?;
        conn.set_selection_owner(window, selection, CURRENT_TIME).map_err(x_err)?.check().map_err(x_err)?;
        let owner = conn.get_selection_owner(selection).map_err(x_err)?.reply().map_err(x_err)?.owner;
        if owner != window {
            return Err("another XSETTINGS manager runs".into());
        }
        // Tell clients waiting for a manager (ICCCM manager selections).
        let event = ClientMessageEvent::new(32, root, manager, [CURRENT_TIME, selection, window, 0, 0]);
        conn.send_event(false, root, EventMask::STRUCTURE_NOTIFY, event).map_err(x_err)?;
        conn.flush().map_err(x_err)?;
        let (jobs, todo) = std::sync::mpsc::channel::<Publish>();
        std::thread::Builder::new()
            .name("helix-xsettings".into())
            .spawn(move || {
                while let Ok(job) = todo.recv() {
                    if let Err(err) = publish(&conn, root, window, settings_atom, resource_manager, job) {
                        tracing::warn!(%err, "cannot publish XSETTINGS");
                    }
                }
            })
            .map_err(x_err)?;
        Ok(Self { serial: 0, published: None, jobs })
    }

    /// Publish `settings` at Xwayland scale `scale` (only what changed).
    pub fn publish(&mut self, settings: &Settings, scale: i32) -> Result<(), String> {
        let values = xs::values(settings, scale);
        let unchanged = xs::encode(&values, 0);
        let settings_bytes = if self.published.as_ref() != Some(&unchanged) {
            self.serial = self.serial.wrapping_add(1);
            self.published = Some(unchanged);
            Some(xs::encode(&values, self.serial))
        } else {
            None
        };
        self.jobs.send(Publish { settings: settings_bytes, resources: xs::resources(settings, scale) }).map_err(|_| "the XSETTINGS thread is gone".to_owned())
    }
}

/// Publishes `job` (on the XSETTINGS thread).
fn publish(conn: &RustConnection, root: Window, window: Window, settings_atom: Atom, resource_manager: Atom, job: Publish) -> Result<(), String> {
    if let Some(bytes) = job.settings {
        conn.change_property(PropMode::REPLACE, window, settings_atom, settings_atom, 8, bytes.len() as u32, &bytes).map_err(x_err)?;
    }
    let existing = conn
        .get_property(false, root, resource_manager, AtomEnum::STRING, 0, 1 << 20)
        .map_err(x_err)?
        .reply()
        .map(|r| String::from_utf8_lossy(&r.value).into_owned())
        .unwrap_or_default();
    let text = xs::merge_resources(&existing, &job.resources);
    if text != existing {
        conn.change_property(PropMode::REPLACE, root, resource_manager, AtomEnum::STRING, 8, text.len() as u32, text.as_bytes()).map_err(x_err)?;
    }
    conn.flush().map_err(x_err)?;
    Ok(())
}

impl Helix {
    /// Xwayland is up on `:display`: become its XSETTINGS manager and
    /// scale it.
    pub fn start_x11_settings(&mut self, display: u32) {
        match XSettings::start(display) {
            Ok(manager) => self.xsettings = Some(manager),
            Err(err) => tracing::warn!(%err, "cannot publish XSETTINGS; X11 apps keep their own settings"),
        }
        self.xwayland_scale = 0;
        self.apply_x11_settings();
    }

    /// Read the settings X11 apps follow again, and apply them.
    pub fn reread_x11_settings(&mut self) {
        self.x11_settings = xs::read();
        self.apply_x11_settings();
    }

    /// Scale Xwayland as the settings and monitors say, and publish the
    /// XSETTINGS and X resources.
    pub fn apply_x11_settings(&mut self) {
        let Some(client) = self.xwayland_client.clone() else { return };
        let scales: Vec<f64> = self.space.outputs().map(|o| o.current_scale().fractional_scale()).collect();
        let scale = xs::xwayland_scale(self.x11_settings.xwayland_scaling, &scales);
        if scale != self.xwayland_scale {
            tracing::info!(scale, "Xwayland scale");
            self.xwayland_scale = scale;
            if let Some(data) = client.get_data::<XWaylandClientData>() {
                data.compositor_state.set_client_scale(f64::from(scale));
            }
            // Outputs tell Xwayland their size in its coordinates again.
            for output in self.space.outputs().cloned().collect::<Vec<_>>() {
                output.change_current_state(None, None, None, None);
            }
            // X11 windows learn their place in the new coordinates.
            let x11: Vec<_> = self.space.elements().filter_map(|w| w.x11_surface().cloned().map(|s| (w.clone(), s))).collect();
            for (window, surface) in x11 {
                if surface.is_override_redirect() {
                    continue;
                }
                let Some(at) = self.space.element_location(&window) else { continue };
                let mut geometry = surface.last_configure();
                geometry.loc = at;
                let _ = surface.configure(Some(geometry));
            }
        }
        if let Some(manager) = self.xsettings.as_mut() {
            if let Err(err) = manager.publish(&self.x11_settings, scale) {
                tracing::warn!(%err, "cannot publish XSETTINGS");
            }
        }
        // The X root cursor, in X11 pixels.
        self.set_x11_root_cursor();
    }
}
