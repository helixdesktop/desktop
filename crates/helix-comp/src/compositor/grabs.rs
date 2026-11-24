//! Interactive move and resize grabs, of the pointer (started by
//! xdg_toplevel move/resize requests, the window menu and X11 apps) and of
//! a touch (xdg_toplevel move/resize requests and X11 apps'
//! `_NET_WM_MOVERESIZE` from a touch). Modeled on
//! Smithay's `smallvil` and `anvil` examples (MIT).

use super::window::Window;
use smithay::input::pointer::{
    AxisFrame, ButtonEvent, GestureHoldBeginEvent, GestureHoldEndEvent, GesturePinchBeginEvent,
    GesturePinchEndEvent, GesturePinchUpdateEvent, GestureSwipeBeginEvent, GestureSwipeEndEvent,
    GestureSwipeUpdateEvent, GrabStartData, MotionEvent, PointerGrab, PointerInnerHandle,
    RelativeMotionEvent,
};
use smithay::input::touch::{self, TouchGrab, TouchInnerHandle};
use smithay::reexports::wayland_protocols::xdg::shell::server::xdg_toplevel;
use smithay::utils::{Logical, Point, Rectangle, Size};
use smithay::wayland::compositor::with_states;
use smithay::wayland::shell::xdg::SurfaceCachedState;

use super::wm::ConfigureWhenReady;
use super::Helix;


/// Forward the grab methods that need no special handling.
macro_rules! forward_passthrough {
    () => {
        fn relative_motion(
            &mut self,
            data: &mut Helix,
            handle: &mut PointerInnerHandle<'_, Helix>,
            focus: Option<(super::focus::PointerFocusTarget, Point<f64, Logical>)>,
            event: &RelativeMotionEvent,
        ) {
            handle.relative_motion(data, focus, event);
        }
        fn axis(&mut self, data: &mut Helix, handle: &mut PointerInnerHandle<'_, Helix>, details: AxisFrame) {
            handle.axis(data, details)
        }
        fn frame(&mut self, data: &mut Helix, handle: &mut PointerInnerHandle<'_, Helix>) {
            handle.frame(data);
        }
        fn gesture_swipe_begin(&mut self, data: &mut Helix, handle: &mut PointerInnerHandle<'_, Helix>, event: &GestureSwipeBeginEvent) {
            handle.gesture_swipe_begin(data, event)
        }
        fn gesture_swipe_update(&mut self, data: &mut Helix, handle: &mut PointerInnerHandle<'_, Helix>, event: &GestureSwipeUpdateEvent) {
            handle.gesture_swipe_update(data, event)
        }
        fn gesture_swipe_end(&mut self, data: &mut Helix, handle: &mut PointerInnerHandle<'_, Helix>, event: &GestureSwipeEndEvent) {
            handle.gesture_swipe_end(data, event)
        }
        fn gesture_pinch_begin(&mut self, data: &mut Helix, handle: &mut PointerInnerHandle<'_, Helix>, event: &GesturePinchBeginEvent) {
            handle.gesture_pinch_begin(data, event)
        }
        fn gesture_pinch_update(&mut self, data: &mut Helix, handle: &mut PointerInnerHandle<'_, Helix>, event: &GesturePinchUpdateEvent) {
            handle.gesture_pinch_update(data, event)
        }
        fn gesture_pinch_end(&mut self, data: &mut Helix, handle: &mut PointerInnerHandle<'_, Helix>, event: &GesturePinchEndEvent) {
            handle.gesture_pinch_end(data, event)
        }
        fn gesture_hold_begin(&mut self, data: &mut Helix, handle: &mut PointerInnerHandle<'_, Helix>, event: &GestureHoldBeginEvent) {
            handle.gesture_hold_begin(data, event)
        }
        fn gesture_hold_end(&mut self, data: &mut Helix, handle: &mut PointerInnerHandle<'_, Helix>, event: &GestureHoldEndEvent) {
            handle.gesture_hold_end(data, event)
        }
        fn start_data(&self) -> &GrabStartData<Helix> {
            &self.start_data
        }
    };
}

/// Drags a window with the pointer.
pub struct MoveGrab {
    pub start_data: GrabStartData<Helix>,
    pub window: Window,
    pub initial_location: Point<i32, Logical>,
    /// Following the pointer with no button held (window menu Move): a
    /// click ends it.
    pub follow: bool,
    /// The window is maximized: restore it once the pointer has moved.
    pub unmaximize: bool,
    /// Ended by its own button (which applies the tile preview); else,
    /// ended from outside (the session locked, another grab), the
    /// preview is dropped.
    pub ended: bool,
}

/// The pointer or finger that grabbed `window` at `start` is now at `at`:
/// the window follows it (a maximized one is restored first, once it has
/// really moved).
fn drag_window(
    data: &mut Helix,
    window: &Window,
    initial_location: &mut Point<i32, Logical>,
    unmaximize: &mut bool,
    start: Point<f64, Logical>,
    at: Point<f64, Logical>,
) {
    let mut delta = at - start;
    if *unmaximize {
        if delta.x.abs() + delta.y.abs() < 8.0 {
            return;
        }
        *unmaximize = false;
        // Under the pointer where it was grabbed (not back where it
        // was before it was maximized).
        data.restore_window_at(window, Some(start));
        *initial_location = data.space.element_location(window).unwrap_or(*initial_location);
        delta = at - start;
    }
    let location = (initial_location.to_f64() + delta).to_i32_round();
    data.space.map_element(window.clone(), location, true);
    data.restack_above();
    data.move_attached(window, location);
    data.update_tile_preview(at);
    // X11 apps need to know where they are (for menus and input).
    if let Some(x11) = window.x11_surface() {
        let _ = x11.configure(Rectangle::new(location, x11.last_configure().size));
    }
}

impl PointerGrab<Helix> for MoveGrab {
    fn motion(
        &mut self,
        data: &mut Helix,
        handle: &mut PointerInnerHandle<'_, Helix>,
        _focus: Option<(super::focus::PointerFocusTarget, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        drag_window(data, &self.window, &mut self.initial_location, &mut self.unmaximize, self.start_data.location, event.location);
    }

    fn button(&mut self, data: &mut Helix, handle: &mut PointerInnerHandle<'_, Helix>, event: &ButtonEvent) {
        if self.follow {
            if event.state == smithay::backend::input::ButtonState::Pressed {
                self.ended = true;
                handle.unset_grab(self, data, event.serial, event.time, true);
                data.finish_tile_preview(&self.window);
            }
            return;
        }
        handle.button(data, event);
        if !handle.current_pressed().contains(&self.start_data.button) {
            data.finish_tile_preview(&self.window);
            tracing::debug!(
                start = ?self.start_data.location,
                initial = ?self.initial_location,
                end = ?data.space.element_location(&self.window),
                "move finished"
            );
            self.ended = true;
            handle.unset_grab(self, data, event.serial, event.time, true);
        }
    }

    forward_passthrough!();

    /// However it ends (helixdesktop/desktop#239: the session locking
    /// during a window-menu Move left the keyboard driving it, so the lock
    /// screen got no keys).
    fn unset(&mut self, data: &mut Helix) {
        if self.follow {
            data.follow = None;
        }
        if !self.ended {
            data.cancel_tile_preview();
        }
    }
}

/// Which edges are being dragged (xdg_toplevel resize edge bits).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edges(u32);

impl Edges {
    /// From xdg_toplevel resize edge bits (top 1, bottom 2, left 4, right 8).
    pub fn from_bits(bits: u32) -> Self {
        Self(bits)
    }

    const TOP: u32 = 1;
    const BOTTOM: u32 = 2;
    const LEFT: u32 = 4;
    const RIGHT: u32 = 8;

    pub fn top(self) -> bool {
        self.0 & Self::TOP != 0
    }
    pub fn bottom(self) -> bool {
        self.0 & Self::BOTTOM != 0
    }
    pub fn left(self) -> bool {
        self.0 & Self::LEFT != 0
    }
    pub fn right(self) -> bool {
        self.0 & Self::RIGHT != 0
    }
}

impl From<xdg_toplevel::ResizeEdge> for Edges {
    fn from(edge: xdg_toplevel::ResizeEdge) -> Self {
        Self(edge as u32)
    }
}

/// An interactive resize in progress. Kept on the compositor state until
/// the client's final commit, so dragging a top or left edge can keep the
/// opposite edge in place once the new size is known.
#[derive(Debug, Clone)]
pub struct ResizeTracking {
    pub window: Window,
    pub edges: Edges,
    pub initial: Rectangle<i32, Logical>,
    pub finished: bool,
}

/// Resizes a window with the pointer.
pub struct ResizeGrab {
    pub start_data: GrabStartData<Helix>,
    pub window: Window,
    pub edges: Edges,
    pub initial: Rectangle<i32, Logical>,
    pub last_size: Size<i32, Logical>,
    /// Following the pointer with no button held: a click ends it.
    pub follow: bool,
}

/// New size for a drag of `delta` on `edges`, clamped to the client's
/// min/max size (0 meaning unbounded).
pub fn resized(
    initial: Size<i32, Logical>,
    edges: Edges,
    delta: Point<f64, Logical>,
    min: Size<i32, Logical>,
    max: Size<i32, Logical>,
) -> Size<i32, Logical> {
    let mut w = initial.w;
    let mut h = initial.h;
    if edges.left() {
        w = (f64::from(initial.w) - delta.x) as i32;
    } else if edges.right() {
        w = (f64::from(initial.w) + delta.x) as i32;
    }
    if edges.top() {
        h = (f64::from(initial.h) - delta.y) as i32;
    } else if edges.bottom() {
        h = (f64::from(initial.h) + delta.y) as i32;
    }
    let clamp = |v: i32, lo: i32, hi: i32| v.max(lo.max(1)).min(if hi == 0 { i32::MAX } else { hi });
    (clamp(w, min.w, max.w), clamp(h, min.h, max.h)).into()
}

/// An X11 window's `edges` dragged by `delta` from `initial`: its new
/// geometry applied directly, keeping the opposite edges in place. The
/// new size.
fn resize_x11(
    data: &mut Helix,
    window: &Window,
    edges: Edges,
    initial: Rectangle<i32, Logical>,
    delta: Point<f64, Logical>,
) -> Size<i32, Logical> {
    let Some(x11) = window.x11_surface() else { return initial.size };
    let (min, max) = (
        x11.min_size().unwrap_or_default(),
        x11.max_size().unwrap_or_default(),
    );
    let size = resized(initial.size, edges, delta, min, max);
    let mut location = initial.loc;
    if edges.left() {
        location.x += initial.size.w - size.w;
    }
    if edges.top() {
        location.y += initial.size.h - size.h;
    }
    let _ = x11.configure(Rectangle::new(location, size));
    data.space.map_element(window.clone(), location, true);
    data.restack_above();
    size
}

/// `window`'s `edges` dragged by `delta` from `initial`, by the pointer
/// or a finger. An X11 window gets its new geometry at once; an xdg one is
/// asked for its new size (Resizing), and [`Helix::track_resize_commit`]
/// keeps its opposite edges in place once it draws at that size. The new
/// size.
fn resize_window(
    data: &mut Helix,
    window: &Window,
    edges: Edges,
    initial: Rectangle<i32, Logical>,
    delta: Point<f64, Logical>,
) -> Size<i32, Logical> {
    if window.x11_surface().is_some() {
        return resize_x11(data, window, edges, initial, delta);
    }
    let Some(toplevel) = window.toplevel() else { return initial.size };
    let (min, max) = with_states(toplevel.wl_surface(), |states| {
        let mut cached = states.cached_state.get::<SurfaceCachedState>();
        let current = cached.current();
        (current.min_size, current.max_size)
    });
    let size = resized(initial.size, edges, delta, min, max);
    toplevel.with_pending_state(|state| {
        state.states.set(xdg_toplevel::State::Resizing);
        state.size = Some(size);
    });
    toplevel.configure_pending();
    size
}

/// A resize of `window` is over, at `last_size`: an xdg window is no
/// longer Resizing (unless `cancelled`, which puts the window back
/// itself), and the tracking is finished, so later commits don't
/// re-anchor the window (helixdesktop/desktop#239).
fn end_resize(data: &mut Helix, window: &Window, last_size: Size<i32, Logical>, cancelled: bool) {
    if !cancelled {
        if let Some(toplevel) = window.toplevel() {
            toplevel.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Resizing);
                state.size = Some(last_size);
            });
            toplevel.configure_pending();
        }
    }
    if let Some(tracking) = data.resize.as_mut().filter(|t| &t.window == window) {
        tracking.finished = true;
    }
}

impl PointerGrab<Helix> for ResizeGrab {
    fn motion(
        &mut self,
        data: &mut Helix,
        handle: &mut PointerInnerHandle<'_, Helix>,
        _focus: Option<(super::focus::PointerFocusTarget, Point<f64, Logical>)>,
        event: &MotionEvent,
    ) {
        handle.motion(data, None, event);
        let delta = event.location - self.start_data.location;
        self.last_size = resize_window(data, &self.window, self.edges, self.initial, delta);
    }

    fn button(&mut self, data: &mut Helix, handle: &mut PointerInnerHandle<'_, Helix>, event: &ButtonEvent) {
        if self.follow {
            if event.state != smithay::backend::input::ButtonState::Pressed {
                return;
            }
        } else {
            handle.button(data, event);
            if handle.current_pressed().contains(&self.start_data.button) {
                return;
            }
        }
        handle.unset_grab(self, data, event.serial, event.time, true);
    }

    forward_passthrough!();

    /// However it ends: no longer Resizing, at the last size, and the
    /// tracking finished, so later commits don't re-anchor the window
    /// (helixdesktop/desktop#239). Escape (`cancel_follow`) takes `follow`
    /// first and puts the window back itself.
    fn unset(&mut self, data: &mut Helix) {
        let cancelled = self.follow && data.follow.is_none();
        if self.follow {
            data.follow = None;
        }
        end_resize(data, &self.window, self.last_size, cancelled);
    }
}

/// Forward the touch grab methods a move or resize leaves alone. Other
/// touch points than the one that started it go nowhere while it lasts.
macro_rules! forward_touch_passthrough {
    () => {
        fn down(
            &mut self,
            _data: &mut Helix,
            _handle: &mut TouchInnerHandle<'_, Helix>,
            _focus: Option<(super::focus::PointerFocusTarget, Point<f64, Logical>)>,
            _event: &touch::DownEvent,
        ) {
        }
        fn frame(&mut self, data: &mut Helix, handle: &mut TouchInnerHandle<'_, Helix>) {
            handle.frame(data);
        }
        fn cancel(&mut self, data: &mut Helix, handle: &mut TouchInnerHandle<'_, Helix>) {
            handle.cancel(data);
            handle.unset_grab(self, data);
        }
        fn shape(&mut self, data: &mut Helix, handle: &mut TouchInnerHandle<'_, Helix>, event: &touch::ShapeEvent) {
            handle.shape(data, event);
        }
        fn orientation(&mut self, data: &mut Helix, handle: &mut TouchInnerHandle<'_, Helix>, event: &touch::OrientationEvent) {
            handle.orientation(data, event);
        }
        fn start_data(&self) -> &touch::GrabStartData<Helix> {
            &self.start_data
        }
    };
}

/// Drags a window with a finger (an app's `xdg_toplevel.move` with a touch
/// serial, or an X11 app's `_NET_WM_MOVERESIZE` from a touch, as mutter
/// and wlroots): the touch point that was down when it asked, until it
/// lifts.
pub struct TouchMoveGrab {
    pub start_data: touch::GrabStartData<Helix>,
    pub window: Window,
    pub initial_location: Point<i32, Logical>,
    /// The window is maximized: restore it once the finger has moved.
    pub unmaximize: bool,
    /// Ended by its own finger lifting (which applies the tile preview).
    pub ended: bool,
}

impl TouchGrab<Helix> for TouchMoveGrab {
    fn motion(
        &mut self,
        data: &mut Helix,
        _handle: &mut TouchInnerHandle<'_, Helix>,
        _focus: Option<(super::focus::PointerFocusTarget, Point<f64, Logical>)>,
        event: &touch::MotionEvent,
    ) {
        if event.slot != self.start_data.slot {
            return;
        }
        drag_window(data, &self.window, &mut self.initial_location, &mut self.unmaximize, self.start_data.location, event.location);
    }

    fn up(&mut self, data: &mut Helix, handle: &mut TouchInnerHandle<'_, Helix>, event: &touch::UpEvent) {
        if event.slot != self.start_data.slot {
            return;
        }
        handle.up(data, event);
        data.finish_tile_preview(&self.window);
        self.ended = true;
        handle.unset_grab(self, data);
    }

    forward_touch_passthrough!();

    fn unset(&mut self, data: &mut Helix) {
        if !self.ended {
            data.cancel_tile_preview();
        }
    }
}

/// Resizes a window with a finger (`xdg_toplevel.resize` with a touch
/// serial, or `_NET_WM_MOVERESIZE` from a touch), as [`TouchMoveGrab`],
/// the same way as [`ResizeGrab`].
pub struct TouchResizeGrab {
    pub start_data: touch::GrabStartData<Helix>,
    pub window: Window,
    pub edges: Edges,
    pub initial: Rectangle<i32, Logical>,
    pub last_size: Size<i32, Logical>,
}

impl TouchGrab<Helix> for TouchResizeGrab {
    fn motion(
        &mut self,
        data: &mut Helix,
        _handle: &mut TouchInnerHandle<'_, Helix>,
        _focus: Option<(super::focus::PointerFocusTarget, Point<f64, Logical>)>,
        event: &touch::MotionEvent,
    ) {
        if event.slot != self.start_data.slot {
            return;
        }
        self.last_size = resize_window(data, &self.window, self.edges, self.initial, event.location - self.start_data.location);
    }

    fn up(&mut self, data: &mut Helix, handle: &mut TouchInnerHandle<'_, Helix>, event: &touch::UpEvent) {
        if event.slot != self.start_data.slot {
            return;
        }
        handle.up(data, event);
        handle.unset_grab(self, data);
    }

    forward_touch_passthrough!();

    /// However it ends, as [`ResizeGrab`].
    fn unset(&mut self, data: &mut Helix) {
        end_resize(data, &self.window, self.last_size, false);
    }
}

impl Helix {
    /// A touch move or resize (of `window`, or any) going on ends where it
    /// is: `_NET_WM_MOVERESIZE` cancel, or the session locking.
    pub fn end_touch_move_resize(&mut self, window: Option<&Window>) {
        let Some(touch) = self.seat.get_touch() else { return };
        let ours = touch
            .with_grab(|_, grab| {
                let grabbed = grab
                    .downcast_ref::<TouchMoveGrab>()
                    .map(|g| &g.window)
                    .or_else(|| grab.downcast_ref::<TouchResizeGrab>().map(|g| &g.window));
                grabbed.is_some_and(|w| window.is_none_or(|window| w == window))
            })
            .unwrap_or(false);
        if ours {
            touch.unset_grab(self);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_math() {
        let initial: Size<i32, Logical> = (400, 300).into();
        let none: Size<i32, Logical> = (0, 0).into();
        let br = Edges(Edges::BOTTOM | Edges::RIGHT);
        assert_eq!(resized(initial, br, (50.0, 20.0).into(), none, none), (450, 320).into());
        let tl = Edges(Edges::TOP | Edges::LEFT);
        assert_eq!(resized(initial, tl, (50.0, 20.0).into(), none, none), (350, 280).into());
        // Clamped to the client's minimum and maximum.
        let min: Size<i32, Logical> = (380, 0).into();
        let max: Size<i32, Logical> = (0, 310).into();
        assert_eq!(resized(initial, br, (-100.0, 100.0).into(), min, max), (380, 310).into());
        // Never below 1x1.
        assert_eq!(resized(initial, br, (-1000.0, -1000.0).into(), none, none), (1, 1).into());
    }
}
