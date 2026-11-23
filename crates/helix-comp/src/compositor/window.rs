//! Helix's window: what the space holds, an app's Wayland toplevel or an
//! X11 window. The X11 side goes through `X11Window`, so another X11
//! window manager can stand behind it (helixdesktop/desktop#191); today
//! it is Smithay's `X11Surface`.
//!
//! Behaves as Smithay's `desktop::Window` did: equal by identity (each
//! `Window` made is a new one, so an X11 window mapped again is a new
//! window), the same geometry, bounding box, input region, stacking
//! layer and rendering.

use std::borrow::Cow;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use smithay::backend::renderer::element::surface::WaylandSurfaceRenderElement;
use smithay::backend::renderer::element::AsRenderElements;
use smithay::backend::renderer::{ImportAll, Renderer};
use smithay::desktop::space::{RenderZindex, SpaceElement};
use smithay::desktop::utils::{
    send_dmabuf_feedback_surface_tree, send_frames_surface_tree, take_presentation_feedback_surface_tree,
    with_surfaces_surface_tree, OutputPresentationFeedback,
};
use smithay::desktop::{PopupManager, WindowSurfaceType};
use smithay::output::Output;
use smithay::reexports::wayland_protocols::wp::presentation_time::server::wp_presentation_feedback;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::{IsAlive, Logical, Physical, Point, Rectangle, Scale, Size};
use smithay::wayland::compositor::SurfaceData;
use smithay::wayland::dmabuf::DmabufFeedback;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::shell::xdg::ToplevelSurface;
use smithay::xwayland::xwm::{WmWindowType, X11Window as X11WindowId};
use smithay::xwayland::X11Surface;
use x11rb::errors::ConnectionError;

static NEXT_ID: AtomicUsize = AtomicUsize::new(1);

/// An app's window (or an X11 override-redirect window, such as a menu).
#[derive(Debug, Clone)]
pub struct Window(Arc<Inner>);

#[derive(Debug)]
struct Inner {
    id: usize,
    surface: Surface,
}

// An X11Surface carries Smithay's atom table (as in focus.rs).
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
enum Surface {
    Wayland(smithay::desktop::Window),
    X11(X11Window),
}

impl PartialEq for Window {
    fn eq(&self, other: &Self) -> bool {
        self.0.id == other.0.id
    }
}

impl Eq for Window {}

impl std::hash::Hash for Window {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.id.hash(state);
    }
}

impl Window {
    fn new(surface: Surface) -> Self {
        Self(Arc::new(Inner { id: NEXT_ID.fetch_add(1, Ordering::Relaxed), surface }))
    }

    /// An app's Wayland toplevel.
    pub fn new_wayland_window(toplevel: ToplevelSurface) -> Self {
        Self::new(Surface::Wayland(smithay::desktop::Window::new_wayland_window(toplevel)))
    }

    /// An X11 window (each call makes a new window).
    pub fn new_x11_window(x11: X11Window) -> Self {
        Self::new(Surface::X11(x11))
    }

    /// The app's Wayland toplevel, for a Wayland window.
    pub fn toplevel(&self) -> Option<&ToplevelSurface> {
        match &self.0.surface {
            Surface::Wayland(w) => w.toplevel(),
            Surface::X11(_) => None,
        }
    }

    /// The X11 window, for an X11 one.
    pub fn x11_surface(&self) -> Option<&X11Window> {
        match &self.0.surface {
            Surface::Wayland(_) => None,
            Surface::X11(x) => Some(x),
        }
    }

    /// The window's geometry (without client-side shadows).
    pub fn geometry(&self) -> Rectangle<i32, Logical> {
        match &self.0.surface {
            Surface::Wayland(w) => w.geometry(),
            Surface::X11(x) => x.space_geometry(),
        }
    }

    /// A bounding box over the window and its subsurfaces.
    pub fn bbox(&self) -> Rectangle<i32, Logical> {
        match &self.0.surface {
            Surface::Wayland(w) => w.bbox(),
            Surface::X11(x) => x.bbox(),
        }
    }

    /// The bounding box with the window's popups.
    pub fn bbox_with_popups(&self) -> Rectangle<i32, Logical> {
        match &self.0.surface {
            Surface::Wayland(w) => w.bbox_with_popups(),
            Surface::X11(x) => {
                let mut bbox = x.bbox();
                if let Some(surface) = x.wl_surface() {
                    for (popup, location) in PopupManager::popups_for_surface(&surface) {
                        let offset = self.geometry().loc + location - popup.geometry().loc;
                        bbox = bbox.merge(smithay::desktop::utils::bbox_from_surface_tree(popup.wl_surface(), offset));
                    }
                }
                bbox
            }
        }
    }

    /// Activates or deactivates the window; whether that changed it.
    pub fn set_activated(&self, active: bool) -> bool {
        match &self.0.surface {
            Surface::Wayland(w) => w.set_activated(active),
            Surface::X11(x) => {
                let already = x.is_activated();
                x.set_activated(active).is_ok() && (already ^ active)
            }
        }
    }

    /// Runs `processor` on every surface of the window and its popups.
    pub fn with_surfaces<F>(&self, mut processor: F)
    where
        F: FnMut(&WlSurface, &SurfaceData),
    {
        match &self.0.surface {
            Surface::Wayland(w) => w.with_surfaces(processor),
            Surface::X11(_) => {
                if let Some(surface) = self.wl_surface() {
                    with_surfaces_surface_tree(&surface, &mut processor);
                    for (popup, _) in PopupManager::popups_for_surface(&surface) {
                        with_surfaces_surface_tree(popup.wl_surface(), &mut processor);
                    }
                }
            }
        }
    }

    /// Frame callbacks for the window's surfaces shown on `output`.
    pub fn send_frame<T, F>(&self, output: &Output, time: T, throttle: Option<Duration>, primary_scan_out_output: F)
    where
        T: Into<Duration>,
        F: FnMut(&WlSurface, &SurfaceData) -> Option<Output> + Copy,
    {
        match &self.0.surface {
            Surface::Wayland(w) => w.send_frame(output, time, throttle, primary_scan_out_output),
            Surface::X11(_) => {
                let time = time.into();
                if let Some(surface) = self.wl_surface() {
                    send_frames_surface_tree(&surface, output, time, throttle, primary_scan_out_output);
                    for (popup, _) in PopupManager::popups_for_surface(&surface) {
                        send_frames_surface_tree(popup.wl_surface(), output, time, throttle, primary_scan_out_output);
                    }
                }
            }
        }
    }

    /// Dmabuf feedback for the window's surfaces.
    pub fn send_dmabuf_feedback<'a, P, F>(&self, output: &Output, primary_scan_out_output: P, select_dmabuf_feedback: F)
    where
        P: FnMut(&WlSurface, &SurfaceData) -> Option<Output> + Copy,
        F: Fn(&WlSurface, &SurfaceData) -> &'a DmabufFeedback + Copy,
    {
        match &self.0.surface {
            Surface::Wayland(w) => w.send_dmabuf_feedback(output, primary_scan_out_output, select_dmabuf_feedback),
            Surface::X11(_) => {
                if let Some(surface) = self.wl_surface() {
                    send_dmabuf_feedback_surface_tree(&surface, output, primary_scan_out_output, select_dmabuf_feedback);
                    for (popup, _) in PopupManager::popups_for_surface(&surface) {
                        send_dmabuf_feedback_surface_tree(popup.wl_surface(), output, primary_scan_out_output, select_dmabuf_feedback);
                    }
                }
            }
        }
    }

    /// Takes the presentation feedback of the window's surfaces.
    pub fn take_presentation_feedback<F1, F2>(&self, output_feedback: &mut OutputPresentationFeedback, primary_scan_out_output: F1, presentation_feedback_flags: F2)
    where
        F1: FnMut(&WlSurface, &SurfaceData) -> Option<Output> + Copy,
        F2: FnMut(&WlSurface, &SurfaceData) -> wp_presentation_feedback::Kind + Copy,
    {
        match &self.0.surface {
            Surface::Wayland(w) => w.take_presentation_feedback(output_feedback, primary_scan_out_output, presentation_feedback_flags),
            Surface::X11(_) => {
                if let Some(surface) = self.wl_surface() {
                    take_presentation_feedback_surface_tree(&surface, output_feedback, primary_scan_out_output, presentation_feedback_flags);
                    for (popup, _) in PopupManager::popups_for_surface(&surface) {
                        take_presentation_feedback_surface_tree(popup.wl_surface(), output_feedback, primary_scan_out_output, presentation_feedback_flags);
                    }
                }
            }
        }
    }

    /// Updates the window's bounding box after a commit.
    pub fn on_commit(&self) {
        if let Surface::Wayland(w) = &self.0.surface {
            w.on_commit();
        }
    }

    /// The topmost surface under `point` (relative to the window) taking
    /// input there, and where it is.
    pub fn surface_under<P: Into<Point<f64, Logical>>>(&self, point: P, surface_type: WindowSurfaceType) -> Option<(WlSurface, Point<i32, Logical>)> {
        match &self.0.surface {
            Surface::Wayland(w) => w.surface_under(point, surface_type),
            Surface::X11(x) => {
                if surface_type.contains(WindowSurfaceType::TOPLEVEL) {
                    x.surface_under(point.into(), surface_type)
                } else {
                    None
                }
            }
        }
    }
}

impl IsAlive for Window {
    fn alive(&self) -> bool {
        match &self.0.surface {
            Surface::Wayland(w) => w.alive(),
            Surface::X11(x) => x.alive(),
        }
    }
}

impl WaylandFocus for Window {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match &self.0.surface {
            Surface::Wayland(w) => w.wl_surface(),
            Surface::X11(x) => x.wl_surface().map(Cow::Owned),
        }
    }
}

impl SpaceElement for Window {
    fn geometry(&self) -> Rectangle<i32, Logical> {
        Window::geometry(self)
    }

    fn bbox(&self) -> Rectangle<i32, Logical> {
        self.bbox_with_popups()
    }

    fn is_in_input_region(&self, point: &Point<f64, Logical>) -> bool {
        self.surface_under(*point, WindowSurfaceType::ALL).is_some()
    }

    fn z_index(&self) -> u8 {
        match &self.0.surface {
            Surface::Wayland(w) => SpaceElement::z_index(w),
            // As Smithay's window did for X11 surfaces too.
            Surface::X11(_) => RenderZindex::Shell as u8,
        }
    }

    fn set_activate(&self, activated: bool) {
        self.set_activated(activated);
    }

    fn output_enter(&self, output: &Output, overlap: Rectangle<i32, Logical>) {
        match &self.0.surface {
            Surface::Wayland(w) => SpaceElement::output_enter(w, output, overlap),
            Surface::X11(x) => x.output_enter(output, overlap),
        }
    }

    fn output_leave(&self, output: &Output) {
        match &self.0.surface {
            Surface::Wayland(w) => SpaceElement::output_leave(w, output),
            Surface::X11(x) => x.output_leave(output),
        }
    }

    fn refresh(&self) {
        match &self.0.surface {
            Surface::Wayland(w) => SpaceElement::refresh(w),
            Surface::X11(x) => x.refresh(),
        }
    }
}

impl<R> AsRenderElements<R> for Window
where
    R: Renderer + ImportAll,
    R::TextureId: Clone + 'static,
{
    type RenderElement = WaylandSurfaceRenderElement<R>;

    fn render_elements<C: From<WaylandSurfaceRenderElement<R>>>(&self, renderer: &mut R, location: Point<i32, Physical>, scale: Scale<f64>, alpha: f32) -> Vec<C> {
        match &self.0.surface {
            Surface::Wayland(w) => AsRenderElements::<R>::render_elements(w, renderer, location, scale, alpha),
            Surface::X11(X11Window::Smithay(s)) => AsRenderElements::<R>::render_elements(s, renderer, location, scale, alpha),
            Surface::X11(X11Window::Helix(w)) => match w.wl_surface() {
                Some(surface) => smithay::backend::renderer::element::surface::render_elements_from_surface_tree(renderer, &surface, location, scale, alpha, smithay::backend::renderer::element::Kind::Unspecified),
                None => Vec::new(),
            },
        }
    }
}

/// An X11 window, as Helix uses it: Smithay's window manager's
/// `X11Surface`, or a window of Helix's own (`HELIX_XWM=helix`, #191).
/// Reached as either only by the window manager glue (`xwayland.rs`) and
/// the focus targets.
// Smithay's X11Surface carries its atom table (as in focus.rs).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum X11Window {
    Smithay(X11Surface),
    Helix(super::xwm::XWin),
}

macro_rules! either {
    ($self:ident, $x:ident => $e:expr) => {
        match $self {
            X11Window::Smithay($x) => $e,
            X11Window::Helix($x) => $e,
        }
    };
}

impl X11Window {
    pub fn new(surface: X11Surface) -> Self {
        Self::Smithay(surface)
    }

    /// Smithay's surface, for its window manager's glue.
    pub fn smithay(&self) -> Option<&X11Surface> {
        match self {
            Self::Smithay(s) => Some(s),
            Self::Helix(_) => None,
        }
    }

    /// Helix's window, for its window manager's glue.
    pub fn helix(&self) -> Option<&super::xwm::XWin> {
        match self {
            Self::Smithay(_) => None,
            Self::Helix(w) => Some(w),
        }
    }

    /// The X window.
    pub fn window_id(&self) -> X11WindowId {
        either!(self, x => x.window_id())
    }

    /// The Wayland surface Xwayland shows it in, once paired.
    pub fn wl_surface(&self) -> Option<WlSurface> {
        either!(self, x => x.wl_surface())
    }

    pub fn alive(&self) -> bool {
        either!(self, x => x.alive())
    }

    pub fn is_override_redirect(&self) -> bool {
        either!(self, x => x.is_override_redirect())
    }

    pub fn title(&self) -> String {
        either!(self, x => x.title())
    }

    pub fn class(&self) -> String {
        either!(self, x => x.class())
    }

    pub fn instance(&self) -> String {
        either!(self, x => x.instance())
    }

    /// `_NET_WM_PID`.
    pub fn pid(&self) -> Option<u32> {
        either!(self, x => x.pid())
    }

    pub fn window_type(&self) -> Option<WmWindowType> {
        either!(self, x => x.window_type())
    }

    /// The window it is transient for (`WM_TRANSIENT_FOR`).
    pub fn transient_for(&self) -> Option<X11WindowId> {
        match self {
            Self::Smithay(s) => s.is_transient_for(),
            Self::Helix(w) => w.transient_for(),
        }
    }

    pub fn min_size(&self) -> Option<Size<i32, Logical>> {
        either!(self, x => x.min_size())
    }

    pub fn max_size(&self) -> Option<Size<i32, Logical>> {
        either!(self, x => x.max_size())
    }

    /// Whether it wants a title bar (`_MOTIF_WM_HINTS`).
    pub fn is_decorated(&self) -> bool {
        either!(self, x => x.is_decorated())
    }

    pub fn is_modal(&self) -> bool {
        either!(self, x => x.is_modal())
    }

    pub fn is_above(&self) -> bool {
        either!(self, x => x.is_above())
    }

    pub fn is_skip_taskbar(&self) -> bool {
        either!(self, x => x.is_skip_taskbar())
    }

    pub fn is_fullscreen(&self) -> bool {
        either!(self, x => x.is_fullscreen())
    }

    pub fn is_maximized(&self) -> bool {
        either!(self, x => x.is_maximized())
    }

    pub fn is_activated(&self) -> bool {
        either!(self, x => x.is_activated())
    }

    /// Whether its `WM_HINTS` urgency flag is set.
    pub fn is_urgent(&self) -> bool {
        match self {
            Self::Smithay(s) => s.hints().is_some_and(|h| h.urgent),
            Self::Helix(w) => w.hints().urgent,
        }
    }

    /// The shadow the app draws around itself (`_GTK_FRAME_EXTENTS`):
    /// left, right, top, bottom, in logical pixels.
    pub fn shadow(&self) -> [i32; 4] {
        match self {
            Self::Smithay(s) => {
                let e = s.frame_extents();
                [e.left, e.right, e.top, e.bottom]
            }
            Self::Helix(w) => w.shadow(),
        }
    }

    /// Whether its `WM_HINTS` asks to start iconic (minimized; ICCCM
    /// 4.1.2.4, XCOM 2).
    pub fn starts_iconic(&self) -> bool {
        match self {
            Self::Smithay(s) => s.hints().is_some_and(|h| matches!(h.initial_state, Some(x11rb::properties::WmHintsState::Iconic))),
            Self::Helix(w) => w.hints().iconic,
        }
    }

    /// The rectangle last given to it.
    pub fn last_configure(&self) -> Rectangle<i32, Logical> {
        either!(self, x => x.last_configure())
    }

    /// The size it was last asked to have, without its shadow: what it
    /// will look like once it has drawn. With Smithay's window manager the
    /// surface (and so `geometry`) keeps the old size until the app draws,
    /// which can be after the next request when its commits are held for
    /// a sync request (#212).
    pub fn requested_size(&self) -> Size<i32, Logical> {
        let size = match self {
            Self::Smithay(s) => s.requested_configure().size,
            Self::Helix(w) => w.last_configure().size,
        };
        let [l, r, t, b] = self.shadow();
        Size::from(((size.w - l - r).max(1), (size.h - t - b).max(1)))
    }

    /// Moves and resizes it.
    pub fn configure(&self, rect: impl Into<Option<Rectangle<i32, Logical>>>) -> Result<(), smithay::xwayland::xwm::X11SurfaceError> {
        match self {
            // Resizes go through _NET_WM_SYNC_REQUEST for apps that take
            // part (Smithay falls back to a plain configure otherwise);
            // one that doesn't answer in time doesn't hold them up.
            Self::Smithay(s) => match rect.into() {
                Some(rect) if !s.is_override_redirect() => s.configure_with_sync(rect, Some(std::time::Duration::from_millis(200))),
                rect => s.configure(rect),
            },
            Self::Helix(w) => w.configure(rect.into()).map_err(Into::into),
        }
    }

    pub fn set_activated(&self, activated: bool) -> Result<(), ConnectionError> {
        either!(self, x => x.set_activated(activated))
    }

    pub fn set_fullscreen(&self, fullscreen: bool) -> Result<(), ConnectionError> {
        either!(self, x => x.set_fullscreen(fullscreen))
    }

    pub fn set_maximized(&self, maximized: bool) -> Result<(), ConnectionError> {
        either!(self, x => x.set_maximized(maximized))
    }

    pub fn set_above(&self, above: bool) -> Result<(), ConnectionError> {
        either!(self, x => x.set_above(above))
    }

    pub fn set_modal(&self, modal: bool) -> Result<(), ConnectionError> {
        either!(self, x => x.set_modal(modal))
    }

    /// Tells it it is minimized or shown again: `WM_STATE` Iconic or
    /// Normal, and `_NET_WM_STATE_HIDDEN` (ICCCM 4.1.4, EWMH). Wine and
    /// Proton games wait for it to draw again after a restore (#194).
    pub fn set_hidden(&self, hidden: bool) -> Result<(), ConnectionError> {
        either!(self, x => x.set_hidden(hidden))
    }

    /// Asks it to close (`WM_DELETE_WINDOW`, or destroys it).
    pub fn close(&self) -> Result<(), ConnectionError> {
        either!(self, x => x.close())
    }

    /// Says how tall the title bar Helix draws above it is
    /// (`_NET_FRAME_EXTENTS`; Helix's window manager only).
    pub fn set_frame_extents(&self, top: i32) {
        if let Self::Helix(w) = self {
            let _ = w.set_frame_extents(top);
        }
    }

    fn bbox(&self) -> Rectangle<i32, Logical> {
        either!(self, x => x.bbox())
    }

    fn space_geometry(&self) -> Rectangle<i32, Logical> {
        match self {
            Self::Smithay(s) => SpaceElement::geometry(s),
            Self::Helix(w) => w.geometry(),
        }
    }

    fn surface_under(&self, point: Point<f64, Logical>, surface_type: WindowSurfaceType) -> Option<(WlSurface, Point<i32, Logical>)> {
        match self {
            Self::Smithay(s) => s.surface_under(point, (0, 0), surface_type),
            Self::Helix(w) => {
                let surface = w.wl_surface()?;
                smithay::desktop::utils::under_from_surface_tree(&surface, point, (0, 0), surface_type)
            }
        }
    }

    fn output_enter(&self, output: &Output, overlap: Rectangle<i32, Logical>) {
        match self {
            Self::Smithay(s) => SpaceElement::output_enter(s, output, overlap),
            Self::Helix(w) => w.output_enter(output, overlap),
        }
    }

    fn output_leave(&self, output: &Output) {
        match self {
            Self::Smithay(s) => SpaceElement::output_leave(s, output),
            Self::Helix(w) => w.output_leave(output),
        }
    }

    fn refresh(&self) {
        match self {
            Self::Smithay(s) => SpaceElement::refresh(s),
            Self::Helix(w) => w.refresh(),
        }
    }
}
