//! Activation tokens for what the shell starts or wakes (helixdesktop/spec
//! `contracts/wayland/README.md`, "Windows wanting attention"): an app
//! launched from the shell, a notification's action. helix-comp grants
//! focus only to `xdg_activation_v1` tokens made by the shell (its
//! privileged client) or for the user's latest key or button press, so
//! an app that is already running and presents a window when asked (a
//! D-Bus activated app, a notification click) needs one of the shell's.
//!
//! Tokens are asked for on an event queue of their own (one
//! `xdg_activation_v1` bound to it), so any thread can make one with a
//! round trip, also from inside the UI's own dispatch. Launches get them
//! through [`launch_context_type`]: a `GAppLaunchContext` whose startup
//! notification id is a fresh token, which GIO passes to D-Bus activated
//! apps (platform data `activation-token`, `desktop-startup-id`) and to
//! spawned ones (`XDG_ACTIVATION_TOKEN`, `DESKTOP_STARTUP_ID`).
//!
//! A launch's token is also announced as one (`gtk_shell1.notify_launch`,
//! as GTK does without `xdg_activation_v1`), so helix-comp shows the busy
//! pointer until the app is up (helixdesktop/spec "Launch feedback");
//! tokens for anything else are not.

use std::sync::{Mutex, OnceLock};

use smithay_client_toolkit::reexports::client::globals::GlobalList;
use smithay_client_toolkit::reexports::client::{Connection, Dispatch, EventQueue, QueueHandle};
use smithay_client_toolkit::reexports::protocols::xdg::activation::v1::client::xdg_activation_token_v1::{self, XdgActivationTokenV1};
use smithay_client_toolkit::reexports::protocols::xdg::activation::v1::client::xdg_activation_v1::XdgActivationV1;
use helix_protocols::gtk_shell_client::gtk_shell1::{self, GtkShell1};

struct Source {
    queue: EventQueue<Minted>,
    activation: XdgActivationV1,
    /// To say a token is a launch's (`notify_launch`, version 3).
    gtk_shell: Option<GtkShell1>,
}

/// The token the compositor sent.
#[derive(Default)]
struct Minted(Option<String>);

static SOURCE: OnceLock<Mutex<Source>> = OnceLock::new();

impl Dispatch<XdgActivationV1, ()> for Minted {
    fn event(_: &mut Self, _: &XdgActivationV1, _: <XdgActivationV1 as smithay_client_toolkit::reexports::client::Proxy>::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<GtkShell1, ()> for Minted {
    fn event(_: &mut Self, _: &GtkShell1, _: gtk_shell1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<XdgActivationTokenV1, ()> for Minted {
    fn event(state: &mut Self, _: &XdgActivationTokenV1, event: xdg_activation_token_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let xdg_activation_token_v1::Event::Done { token } = event {
            state.0 = Some(token);
        }
    }
}

/// Make tokens on `conn` from now on (once; without `xdg_activation_v1`
/// there are none).
pub fn init(conn: &Connection, globals: &GlobalList) {
    let queue = conn.new_event_queue::<Minted>();
    match globals.bind::<XdgActivationV1, Minted, ()>(&queue.handle(), 1..=1, ()) {
        Ok(activation) => {
            let gtk_shell = globals.bind::<GtkShell1, Minted, ()>(&queue.handle(), 3..=3, ()).ok();
            let _ = SOURCE.set(Mutex::new(Source { queue, activation, gtk_shell }));
        }
        Err(err) => tracing::warn!(%err, "no xdg_activation_v1: apps the shell starts may not get focus"),
    }
}

/// A fresh activation token for `app_id` (a desktop file id, with or
/// without `.desktop`), or `None` without a compositor connection.
pub fn token(app_id: Option<&str>) -> Option<String> {
    let mut source = SOURCE.get()?.lock().unwrap_or_else(|e| e.into_inner());
    let source = &mut *source;
    let qh = source.queue.handle();
    let token = source.activation.get_activation_token(&qh, ());
    if let Some(id) = app_id {
        token.set_app_id(id.strip_suffix(".desktop").unwrap_or(id).to_owned());
    }
    token.commit();
    let mut minted = Minted::default();
    // The compositor answers the commit at once; a round trip sees it.
    for _ in 0..3 {
        if source.queue.roundtrip(&mut minted).is_err() || minted.0.is_some() {
            break;
        }
    }
    token.destroy();
    if minted.0.is_none() {
        tracing::debug!("no activation token");
    }
    minted.0
}

/// A fresh activation token for launching `app_id` ([`token`]), announced
/// to the compositor as a launch's, so it shows the busy pointer until
/// the app is up.
pub fn launch_token(app_id: Option<&str>) -> Option<String> {
    let token = token(app_id)?;
    if let Some(source) = SOURCE.get() {
        let mut source = source.lock().unwrap_or_else(|e| e.into_inner());
        let source = &mut *source;
        if let Some(gtk_shell) = &source.gtk_shell {
            gtk_shell.notify_launch(token.clone());
            // Before the app (another client) can use the token.
            let _ = source.queue.roundtrip(&mut Minted::default());
        }
    }
    Some(token)
}

/// [`token`] from async code (the round trip blocks a worker thread).
pub async fn token_async(app_id: Option<String>) -> Option<String> {
    SOURCE.get()?;
    tokio::task::spawn_blocking(move || token(app_id.as_deref())).await.ok().flatten()
}

/// The `GAppLaunchContext` subclass whose `get_startup_notify_id` is a
/// fresh [`token`] for the app launched (gio-rs can't subclass launch
/// contexts, so the class is made with GObject's C API).
pub fn launch_context_type() -> gio::glib::Type {
    use gio::glib::translate::FromGlib;
    static TYPE: OnceLock<usize> = OnceLock::new();
    let raw = *TYPE.get_or_init(|| {
        // SAFETY: registers a subclass of GAppLaunchContext with the
        // parent's class and instance sizes (it adds no fields) and a
        // class_init that only sets a vfunc of the parent's class struct.
        unsafe {
            let parent = gio::ffi::g_app_launch_context_get_type();
            let mut query = std::mem::zeroed::<gio::glib::gobject_ffi::GTypeQuery>();
            gio::glib::gobject_ffi::g_type_query(parent, &mut query);
            gio::glib::gobject_ffi::g_type_register_static_simple(parent, c"HelixLaunchContext".as_ptr(), query.class_size, Some(class_init), query.instance_size, None, 0) as usize
        }
    });
    // SAFETY: a registered GType.
    unsafe { gio::glib::Type::from_glib(raw as gio::glib::ffi::GType) }
}

unsafe extern "C" fn class_init(class: gio::glib::ffi::gpointer, _: gio::glib::ffi::gpointer) {
    let class = class as *mut gio::ffi::GAppLaunchContextClass;
    // SAFETY: GObject passes this type's class struct, which starts with
    // GAppLaunchContextClass.
    unsafe { (*class).get_startup_notify_id = Some(startup_notify_id) };
}

unsafe extern "C" fn startup_notify_id(_: *mut gio::ffi::GAppLaunchContext, info: *mut gio::ffi::GAppInfo, _: *mut gio::glib::ffi::GList) -> *mut std::ffi::c_char {
    let id = if info.is_null() {
        None
    } else {
        // SAFETY: GIO passes a valid GAppInfo; its id is owned by it.
        let id = unsafe { gio::ffi::g_app_info_get_id(info) };
        (!id.is_null()).then(|| unsafe { std::ffi::CStr::from_ptr(id) }.to_string_lossy().into_owned())
    };
    match launch_token(id.as_deref()).and_then(|t| std::ffi::CString::new(t).ok()) {
        // GIO frees it with g_free.
        Some(t) => unsafe { gio::glib::ffi::g_strdup(t.as_ptr()) },
        None => std::ptr::null_mut(),
    }
}

#[cfg(test)]
mod tests {
    use gio::prelude::*;

    #[test]
    fn launch_context_without_compositor() {
        let ctx = gio::glib::Object::with_type(super::launch_context_type()).downcast::<gio::AppLaunchContext>().expect("a launch context");
        // No compositor connection here: no token, no id.
        assert_eq!(ctx.startup_notify_id(None::<&gio::AppInfo>, &[]), None);
    }
}
