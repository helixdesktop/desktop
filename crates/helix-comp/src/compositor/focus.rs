//! What has keyboard focus: a Wayland surface, or an X11 window. X11
//! windows need the X input focus too (set when they get keyboard focus,
//! cleared when they lose it), which Smithay's `X11Surface` target does.

use std::borrow::Cow;
use std::sync::Arc;

use smithay::backend::input::{InputTime, KeyState};
use smithay::desktop::PopupKind;
use smithay::input::dnd::{DndFocus, OfferData, Source};
use smithay::input::keyboard::{KeyboardTarget, KeysymHandle, ModifiersState};
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
    GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
    GestureSwipeUpdateEvent, MotionEvent, PointerTarget, RelativeMotionEvent,
};
use smithay::input::touch::{self, FrameMarker, TouchTarget};
use smithay::input::Seat;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::utils::{IsAlive, Logical, Point, Serial};
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::selection::data_device::WlOfferData;
use smithay::xwayland::xwm::XwmOfferData;
use smithay::xwayland::X11Surface;

use super::Helix;

// An X11Surface carries Smithay's atom table, which outgrew clippy's limit
// with the EWMH state atoms (upstream allows it for its WindowSurface too).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum KeyboardFocusTarget {
    Surface(WlSurface),
    X11(X11Surface),
}

impl IsAlive for KeyboardFocusTarget {
    fn alive(&self) -> bool {
        match self {
            Self::Surface(s) => s.alive(),
            Self::X11(x) => x.alive(),
        }
    }
}

impl WaylandFocus for KeyboardFocusTarget {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Surface(s) => Some(Cow::Borrowed(s)),
            Self::X11(x) => x.wl_surface().map(Cow::Owned),
        }
    }
}

impl From<WlSurface> for KeyboardFocusTarget {
    fn from(surface: WlSurface) -> Self {
        Self::Surface(surface)
    }
}

/// Popup grabs hand their keyboard focus to the pointer: always a popup's
/// or a Wayland window's surface (an X11 window has none until Xwayland
/// associates it, and X11 apps don't use xdg popup grabs).
impl From<KeyboardFocusTarget> for WlSurface {
    fn from(focus: KeyboardFocusTarget) -> Self {
        match focus {
            KeyboardFocusTarget::Surface(s) => s,
            KeyboardFocusTarget::X11(x) => x.wl_surface().expect("an X11 window in a popup grab has a surface"),
        }
    }
}

impl From<PopupKind> for KeyboardFocusTarget {
    fn from(popup: PopupKind) -> Self {
        Self::Surface(popup.wl_surface().clone())
    }
}

impl KeyboardTarget<Helix> for KeyboardFocusTarget {
    fn enter(&self, seat: &Seat<Helix>, data: &mut Helix, keys: Vec<KeysymHandle<'_>>, serial: Serial) {
        match self {
            Self::Surface(s) => KeyboardTarget::enter(s, seat, data, keys, serial),
            Self::X11(x) => KeyboardTarget::enter(x, seat, data, keys, serial),
        }
    }

    fn leave(&self, seat: &Seat<Helix>, data: &mut Helix, serial: Serial) {
        match self {
            Self::Surface(s) => KeyboardTarget::leave(s, seat, data, serial),
            Self::X11(x) => KeyboardTarget::leave(x, seat, data, serial),
        }
    }

    fn key(&self, seat: &Seat<Helix>, data: &mut Helix, key: KeysymHandle<'_>, state: KeyState, serial: Serial, time: smithay::backend::input::InputTime) {
        match self {
            Self::Surface(s) => KeyboardTarget::key(s, seat, data, key, state, serial, time),
            Self::X11(x) => KeyboardTarget::key(x, seat, data, key, state, serial, time),
        }
    }

    fn modifiers(&self, seat: &Seat<Helix>, data: &mut Helix, modifiers: ModifiersState, serial: Serial) {
        match self {
            Self::Surface(s) => KeyboardTarget::modifiers(s, seat, data, modifiers, serial),
            Self::X11(x) => KeyboardTarget::modifiers(x, seat, data, modifiers, serial),
        }
    }
}

/// What gets keyboard focus for `window`: its X11 window, or its surface.
pub fn focus_target(window: &super::window::Window) -> Option<KeyboardFocusTarget> {
    match window.x11_surface() {
        Some(x11) => Some(KeyboardFocusTarget::X11(x11.smithay().clone())),
        None => window.wl_surface().map(|s| KeyboardFocusTarget::Surface(s.into_owned())),
    }
}

impl Helix {
    /// The window a focus target belongs to.
    /// The app id of the window with keyboard focus ("" if none), for logs.
    pub fn focused_app(&self) -> String {
        let focus = self.seat.get_keyboard().and_then(|k| k.current_focus());
        focus.and_then(|f| self.window_for_focus(&f)).map(|w| super::window_list::describe(&w).0).unwrap_or_default()
    }

    pub fn window_for_focus(&self, focus: &KeyboardFocusTarget) -> Option<super::window::Window> {
        match focus {
            KeyboardFocusTarget::X11(x11) => self
                .space
                .elements()
                .chain(self.minimized.iter())
                .chain(self.workspaces.parked())
                .find(|w| w.x11_surface().is_some_and(|x| x.smithay() == x11))
                .cloned(),
            KeyboardFocusTarget::Surface(surface) => self.window_for_surface(surface),
        }
    }

    /// The surface with keyboard focus (an X11 window's once it has one).
    pub fn focused_surface(&self) -> Option<WlSurface> {
        let focus = self.seat.get_keyboard()?.current_focus()?;
        focus.wl_surface().map(Cow::into_owned)
    }
}

/// What has pointer or touch focus: a Wayland surface, or an X11 window
/// (its surface). Pointer and touch events go to the surface either way;
/// the difference is drag and drop, where an X11 window gets XDND from
/// Smithay's window manager instead of a Wayland offer that Xwayland
/// would not pass on (#170).
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum PointerFocusTarget {
    Surface(WlSurface),
    X11(X11Surface),
}

impl PointerFocusTarget {
    /// The surface events go to (an X11 window's once Xwayland associated
    /// it).
    pub fn surface(&self) -> Option<Cow<'_, WlSurface>> {
        WaylandFocus::wl_surface(self)
    }

    pub fn into_surface(self) -> Option<WlSurface> {
        match self {
            Self::Surface(s) => Some(s),
            Self::X11(x) => x.wl_surface(),
        }
    }
}

impl IsAlive for PointerFocusTarget {
    fn alive(&self) -> bool {
        match self {
            Self::Surface(s) => s.alive(),
            Self::X11(x) => x.alive(),
        }
    }
}

impl WaylandFocus for PointerFocusTarget {
    fn wl_surface(&self) -> Option<Cow<'_, WlSurface>> {
        match self {
            Self::Surface(s) => Some(Cow::Borrowed(s)),
            Self::X11(x) => x.wl_surface().map(Cow::Owned),
        }
    }
}

impl From<WlSurface> for PointerFocusTarget {
    fn from(surface: WlSurface) -> Self {
        Self::Surface(surface)
    }
}

impl From<PopupKind> for PointerFocusTarget {
    fn from(popup: PopupKind) -> Self {
        Self::Surface(popup.wl_surface().clone())
    }
}

/// Popup grabs hand the pointer's focus to the keyboard.
impl From<PointerFocusTarget> for KeyboardFocusTarget {
    fn from(focus: PointerFocusTarget) -> Self {
        match focus {
            PointerFocusTarget::Surface(s) => Self::Surface(s),
            PointerFocusTarget::X11(x) => Self::X11(x),
        }
    }
}

/// And the keyboard's to the pointer.
impl From<KeyboardFocusTarget> for PointerFocusTarget {
    fn from(focus: KeyboardFocusTarget) -> Self {
        match focus {
            KeyboardFocusTarget::Surface(s) => Self::Surface(s),
            KeyboardFocusTarget::X11(x) => Self::X11(x),
        }
    }
}

macro_rules! each {
    ($self:ident, $t:ident => $e:expr) => {
        match $self {
            PointerFocusTarget::Surface($t) => $e,
            PointerFocusTarget::X11($t) => $e,
        }
    };
}

impl PointerTarget<Helix> for PointerFocusTarget {
    fn enter(&self, seat: &Seat<Helix>, data: &mut Helix, event: &MotionEvent) {
        each!(self, t => PointerTarget::enter(t, seat, data, event))
    }
    fn motion(&self, seat: &Seat<Helix>, data: &mut Helix, event: &MotionEvent) {
        each!(self, t => PointerTarget::motion(t, seat, data, event))
    }
    fn relative_motion(&self, seat: &Seat<Helix>, data: &mut Helix, event: &RelativeMotionEvent) {
        each!(self, t => PointerTarget::relative_motion(t, seat, data, event))
    }
    fn button(&self, seat: &Seat<Helix>, data: &mut Helix, event: &ButtonEvent) {
        each!(self, t => PointerTarget::button(t, seat, data, event))
    }
    fn axis(&self, seat: &Seat<Helix>, data: &mut Helix, frame: AxisFrame) {
        each!(self, t => PointerTarget::axis(t, seat, data, frame))
    }
    fn frame(&self, seat: &Seat<Helix>, data: &mut Helix) {
        each!(self, t => PointerTarget::frame(t, seat, data))
    }
    fn gesture_swipe_begin(&self, seat: &Seat<Helix>, data: &mut Helix, event: &GestureSwipeBeginEvent) {
        each!(self, t => PointerTarget::gesture_swipe_begin(t, seat, data, event))
    }
    fn gesture_swipe_update(&self, seat: &Seat<Helix>, data: &mut Helix, event: &GestureSwipeUpdateEvent) {
        each!(self, t => PointerTarget::gesture_swipe_update(t, seat, data, event))
    }
    fn gesture_swipe_end(&self, seat: &Seat<Helix>, data: &mut Helix, event: &GestureSwipeEndEvent) {
        each!(self, t => PointerTarget::gesture_swipe_end(t, seat, data, event))
    }
    fn gesture_pinch_begin(&self, seat: &Seat<Helix>, data: &mut Helix, event: &GesturePinchBeginEvent) {
        each!(self, t => PointerTarget::gesture_pinch_begin(t, seat, data, event))
    }
    fn gesture_pinch_update(&self, seat: &Seat<Helix>, data: &mut Helix, event: &GesturePinchUpdateEvent) {
        each!(self, t => PointerTarget::gesture_pinch_update(t, seat, data, event))
    }
    fn gesture_pinch_end(&self, seat: &Seat<Helix>, data: &mut Helix, event: &GesturePinchEndEvent) {
        each!(self, t => PointerTarget::gesture_pinch_end(t, seat, data, event))
    }
    fn gesture_hold_begin(&self, seat: &Seat<Helix>, data: &mut Helix, event: &GestureHoldBeginEvent) {
        each!(self, t => PointerTarget::gesture_hold_begin(t, seat, data, event))
    }
    fn gesture_hold_end(&self, seat: &Seat<Helix>, data: &mut Helix, event: &GestureHoldEndEvent) {
        each!(self, t => PointerTarget::gesture_hold_end(t, seat, data, event))
    }
    fn leave(&self, seat: &Seat<Helix>, data: &mut Helix, serial: Serial, time: InputTime) {
        each!(self, t => PointerTarget::leave(t, seat, data, serial, time))
    }
}

impl TouchTarget<Helix> for PointerFocusTarget {
    fn down(&self, seat: &Seat<Helix>, data: &mut Helix, event: &touch::DownEvent) {
        each!(self, t => TouchTarget::down(t, seat, data, event))
    }
    fn up(&self, seat: &Seat<Helix>, data: &mut Helix, event: &touch::UpEvent) {
        each!(self, t => TouchTarget::up(t, seat, data, event))
    }
    fn motion(&self, seat: &Seat<Helix>, data: &mut Helix, event: &touch::MotionEvent) {
        each!(self, t => TouchTarget::motion(t, seat, data, event))
    }
    fn frame(&self, seat: &Seat<Helix>, data: &mut Helix, marker: FrameMarker) {
        each!(self, t => TouchTarget::frame(t, seat, data, marker))
    }
    fn cancel(&self, seat: &Seat<Helix>, data: &mut Helix, marker: FrameMarker) {
        each!(self, t => TouchTarget::cancel(t, seat, data, marker))
    }
    fn shape(&self, seat: &Seat<Helix>, data: &mut Helix, event: &touch::ShapeEvent) {
        each!(self, t => TouchTarget::shape(t, seat, data, event))
    }
    fn orientation(&self, seat: &Seat<Helix>, data: &mut Helix, event: &touch::OrientationEvent) {
        each!(self, t => TouchTarget::orientation(t, seat, data, event))
    }
    fn last_frame(&self, seat: &Seat<Helix>, data: &mut Helix) -> Option<FrameMarker> {
        each!(self, t => TouchTarget::last_frame(t, seat, data))
    }
}

/// A drag's offer to a Wayland surface, or to an X11 window.
pub enum OfferTo<S: Source> {
    Wayland(WlOfferData<S>),
    X11(XwmOfferData<S>),
}

impl<S: Source> OfferData for OfferTo<S> {
    fn disable(&self) {
        match self {
            Self::Wayland(o) => o.disable(),
            Self::X11(o) => o.disable(),
        }
    }
    fn drop(&self) {
        match self {
            Self::Wayland(o) => o.drop(),
            Self::X11(o) => o.drop(),
        }
    }
    fn validated(&self) -> bool {
        match self {
            Self::Wayland(o) => o.validated(),
            Self::X11(o) => o.validated(),
        }
    }
}

impl DndFocus<Helix> for PointerFocusTarget {
    type OfferData<S>
        = OfferTo<S>
    where
        S: Source;

    fn enter<S: Source>(&self, data: &mut Helix, dh: &DisplayHandle, source: Arc<S>, seat: &Seat<Helix>, location: Point<f64, Logical>, serial: &Serial) -> Option<OfferTo<S>> {
        match self {
            Self::Surface(s) => DndFocus::enter(s, data, dh, source, seat, location, serial).map(OfferTo::Wayland),
            Self::X11(x) => DndFocus::enter(x, data, dh, source, seat, location, serial).map(OfferTo::X11),
        }
    }

    fn motion<S: Source>(&self, data: &mut Helix, offer: Option<&mut OfferTo<S>>, seat: &Seat<Helix>, location: Point<f64, Logical>, time: InputTime) {
        match (self, offer) {
            (Self::Surface(s), Some(OfferTo::Wayland(o))) => DndFocus::motion(s, data, Some(o), seat, location, time),
            (Self::Surface(s), None) => DndFocus::<Helix>::motion::<S>(s, data, None, seat, location, time),
            (Self::X11(x), Some(OfferTo::X11(o))) => DndFocus::motion(x, data, Some(o), seat, location, time),
            (Self::X11(x), None) => DndFocus::<Helix>::motion::<S>(x, data, None, seat, location, time),
            _ => {}
        }
    }

    fn leave<S: Source>(&self, data: &mut Helix, offer: Option<&mut OfferTo<S>>, seat: &Seat<Helix>) {
        match (self, offer) {
            (Self::Surface(s), Some(OfferTo::Wayland(o))) => DndFocus::leave(s, data, Some(o), seat),
            (Self::Surface(s), None) => DndFocus::<Helix>::leave::<S>(s, data, None, seat),
            (Self::X11(x), Some(OfferTo::X11(o))) => DndFocus::leave(x, data, Some(o), seat),
            (Self::X11(x), None) => DndFocus::<Helix>::leave::<S>(x, data, None, seat),
            _ => {}
        }
    }

    fn drop<S: Source>(&self, data: &mut Helix, offer: Option<&mut OfferTo<S>>, seat: &Seat<Helix>) {
        match (self, offer) {
            (Self::Surface(s), Some(OfferTo::Wayland(o))) => DndFocus::drop(s, data, Some(o), seat),
            (Self::Surface(s), None) => DndFocus::<Helix>::drop::<S>(s, data, None, seat),
            (Self::X11(x), Some(OfferTo::X11(o))) => DndFocus::drop(x, data, Some(o), seat),
            (Self::X11(x), None) => DndFocus::<Helix>::drop::<S>(x, data, None, seat),
            _ => {}
        }
    }
}

impl Helix {
    /// What gets pointer (or touch) focus for `surface`: the X11 window
    /// whose surface it is, or the surface.
    pub fn pointer_target(&self, surface: WlSurface) -> PointerFocusTarget {
        let x11 = self
            .space
            .elements()
            .filter_map(|w| w.x11_surface())
            .find(|x| x.wl_surface().as_ref() == Some(&surface))
            .map(|x| x.smithay().clone());
        match x11 {
            Some(x) => PointerFocusTarget::X11(x),
            None => PointerFocusTarget::Surface(surface),
        }
    }

    /// The pointer (or touch) focus under `pos`, and where its surface is.
    pub fn pointer_focus_under(&self, pos: Point<f64, Logical>) -> Option<(PointerFocusTarget, Point<f64, Logical>)> {
        self.surface_under(pos).map(|(s, p)| (self.pointer_target(s), p))
    }
}
