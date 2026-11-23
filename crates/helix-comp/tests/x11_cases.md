# X11 window manager edge cases

The edge cases an X11 window manager for Xwayland has to get right. They
come from the bug fixes in Smithay's xwm (`S…`, from its `src/xwayland`
history) and wlroots' xwm (`W…`, from its `xwayland/` history and issues),
merged and described here in our own words. They are the definition of
done for Helix's own window manager (#191): each case Helix can check
headlessly has a test in this directory that asserts what X11 apps (or the
shell) see, not how the window manager does it, so the same tests hold for
Smithay's window manager today and Helix's later.

Status:
- a test name: the test checking it. "(ignored)" means it fails on a
  window manager; the test's `common::known_failure` names which and
  why. "(fixed in #N)" names the issue that made it pass on Smithay's.
- **not applicable**: the case only exists for a design Helix doesn't have
  (frame windows), or contradicts a choice Helix made on purpose.
- **not testable headlessly**: it needs something a headless test can't
  force (timing between Xwayland's two connections, real hardware).

Apps are the ones the upstream reports name.

## Map, unmap, withdraw, `WM_STATE`

| ID | Sources | Scenario | Apps | Status |
|---|---|---|---|---|
| X1 | S1, W11 | Withdrawn per ICCCM 4.1.4 (unmap plus a synthetic UnmapNotify), mapped and withdrawn again before the WM catches up; synthetic unmaps of override-redirect windows aren't requests | Zoom | `x11_map::withdraw_and_map_in_a_row_ends_shown`, `x11_map::a_synthetic_unmap_of_a_menu_is_ignored`, `x11_windows::a_window_shown_and_hidden_at_once_stays_hidden` |
| X2 | S2 | Destroying a frame destroyed the client window inside it | GIMP | not applicable: no frame windows |
| X3 | S3, S37 | MapWindow sent several times without unmapping: listed once | GTK 4 | `x11_map::a_window_mapped_three_times_is_listed_once` (fixed in #213) |
| X4 | S4, W12 | `WM_STATE` Normal on map, Withdrawn on unmap, as a `WM_STATE`-typed pair; `_NET_WM_STATE` removed when withdrawn | Wine | `x11_map::wm_state_follows_the_window`, `x11_windows::a_hidden_window_is_withdrawn`, `x11_map::net_wm_state_is_removed_when_withdrawn` (fixed in #215) |
| X5 | S5 | `WM_HINTS` initial state Iconic: starts minimized, `WM_STATE` Iconic, `_NET_WM_STATE_HIDDEN` | XCOM 2 | `x11_map::a_window_asking_to_start_iconic_starts_minimized` |
| X6 | S6, W61 | Minimized: Iconic and HIDDEN; restored: Normal; focus and fullscreen never rewrite `WM_STATE` | Dishonored 2, Steam | `x11_map::minimize_and_restore` (fixed in #194), `x11_map::focus_and_fullscreen_leave_wm_state_alone` |
| X7 | S7 | `WM_CHANGE_STATE` honors its state: Normal doesn't minimize | | `x11_map::minimize_and_restore` (fixed in #194) |

## Override-redirect

| ID | Sources | Scenario | Apps | Status |
|---|---|---|---|---|
| X8 | S8, W13 | Override-redirect flag changed after creation, either way, before mapping: managed or not by its final value | REAPER, Intel Questa, GIMP, Marvel Rivals launcher | `x11_map::override_redirect_cleared_before_mapping_is_managed`, `x11_map::override_redirect_set_before_mapping_is_left_alone` |
| X9 | W14 | Flag changed between two maps | VLC | `x11_map::override_redirect_changed_between_maps` |
| X10 | S9, W19, W58 | Override-redirect windows are never configured, listed, focused or made active | GIMP submenus | `x11_map::override_redirect_windows_are_left_alone` |
| X11 | S11, S12 | An override-redirect window that moves itself gets the pointer where it is now | | `x11_map::a_menu_that_moves_gets_the_pointer_where_it_is` |
| X12 | W3 | One override-redirect window shown again and again (each time a new surface) | GTK 3 menus | `x11_map::a_menu_shown_again_works_again` |
| X72 | S10 | Override-redirect windows reparented back to the root on unmap | | not applicable: no frame windows |

## Which windows are managed

| ID | Sources | Scenario | Apps | Status |
|---|---|---|---|---|
| X13 | S52 | InputOnly windows are never managed | Chromium drag helpers | `x11_map::input_only_windows_are_not_managed` |
| X14 | S53 | The WM's own windows (check, clipboard, drag proxy) are never listed | | `x11_map::the_window_managers_own_windows_are_not_listed` |
| X15 | W58 | `_NET_CLIENT_LIST` in map order, only while mapped | | `x11_map::the_client_list_is_in_map_order` |
| X16 | S54, W7 | Windows destroyed right after mapping: no server grab left behind, no failure | | `x11_map::windows_destroyed_right_after_mapping` |
| X17 | W2 | Mapped, unmapped and mapped again at once: managed once | GeoGebra (Java) | `x11_map::mapped_unmapped_and_mapped_again_at_once` |
| X18 | W4, W5 | Forged or repeated `WL_SURFACE_ID`/`WL_SURFACE_SERIAL` from any client are ignored | | `x11_map::forged_surface_messages_are_ignored` |
| X19 | W17 | A parent destroyed before its transient dialogs | GIMP | `x11_map::a_parent_destroyed_before_its_dialogs` |
| X20 | W18 | Hundreds of windows at once | PySolFC | `x11_map::hundreds_of_windows` |

## Association with Wayland surfaces

| ID | Sources | Scenario | Apps | Status |
|---|---|---|---|---|
| X76 | S13, W1, W6 | The surface's serial (or id) arriving before or after its first commit, on Xwayland's other connection | st | not testable headlessly: the order between Xwayland's X11 and Wayland connections can't be forced from a client |
| X77 | S14 | Keyboard focus given before the surface is associated: keys lost or stuck | cosmic-comp | not testable headlessly: same ordering |
| X78 | W9 | Associated before the first buffer: shown only once there is one | | not testable headlessly: Xwayland decides when buffers come |

## Focus and input models

| ID | Sources | Scenario | Apps | Status |
|---|---|---|---|---|
| X21 | S27, W32, W47, W51 | ICCCM input models: Passive gets the X focus and no protocol messages; Locally Active the focus and `WM_TAKE_FOCUS`; Globally Active `WM_TAKE_FOCUS` only, its own `SetInputFocus` stands; no `WM_HINTS` or no input flag means input | emacs-lucid, xterm, IntelliJ, Chromium, Electron, sxiv, mupdf | `x11_focus::passive_windows_get_the_input_focus`, `x11_focus::windows_without_an_input_hint_get_the_input_focus`, `x11_focus::locally_active_windows_get_the_focus_and_take_focus`, `x11_focus::globally_active_windows_get_take_focus_and_keep_their_own_focus` |
| X22 | S29 | Focus moving between an app's windows never goes to nothing in between | Unity Editor | `x11_focus::focus_moves_between_an_apps_windows_without_a_gap` |
| X23 | S31 | The app moves the X focus to a child: still the active window | Unity Editor | `x11_focus::focus_on_a_child_keeps_the_window_active` |
| X24 | S32 | X focus set to None or PointerRoot: no active window | Wine/Proton games | `x11_focus::focus_on_nothing_clears_the_active_window` |
| X25 | S33, W49 | FocusIn with detail Pointer is not an activation | VSCode | `x11_focus::the_pointers_window_is_not_activated` |
| X26 | S34, W50 | Focus events of a keyboard grab (mode Grab/Ungrab) change nothing; keys go to the grabbing menu | every toolkit's menus | `x11_focus::a_menu_grabbing_the_keyboard_does_not_change_the_active_window` |
| X27 | W21, W52 | While a Wayland window has focus, X11 keyboard grabs still work (X focus not None) | dmenu | `x11_focus::x11_grabs_work_while_a_wayland_window_has_focus` |
| X28 | W54 | `_NET_WM_STATE_FOCUSED` on the focused window only | Steam | `x11_focus::the_focused_window_is_marked_focused` |
| X29 | W55 | The focused window unmapped and destroyed at once: focus moves on | Firefox | `x11_focus::the_focused_window_destroyed_at_once` |
| X30 | W48, W45 | An app focusing its own other window keeps it (Helix decides by the app's active window, not by PID) | Steam, Zoom | `x11_focus::an_app_focusing_its_own_window_keeps_it` |
| X31 | W48 | Quick focus changes settle; WM and app don't chase each other | Chrome | `x11_focus::quick_focus_changes_settle` |
| X32 | S25, S38 | Activating the active window again rewrites nothing (`_NET_WM_STATE`, stacking, configure) | | `x11_focus::activating_the_active_window_again_changes_nothing` |
| X33 | S39, W63 | Pings only for windows listing `_NET_WM_PING` | | `x11_focus::only_windows_listing_ping_are_pinged` |
| X34 | S55, W26 | Changes the compositor makes reach X11 apps at once (flushed), without other X11 traffic | evince, GTK drag and drop | `x11_focus::focus_changes_reach_x11_apps_at_once` |
| X90 | W53 | `_NET_ACTIVE_WINDOW` names a dummy window instead of None while a Wayland window is active | Wine | not applicable: Helix sets None (#166, `x11_windows::the_active_window_follows_focus`); revisit if Wine misbehaves |
| X99 | S35, W62 | `_NET_ACTIVE_WINDOW` requests from apps and pagers | | `x11_windows::activation_requests` |
| X100 | S28 | Keys reach an app whose focused window takes no input | JetBrains | `x11_windows::typing_reaches_an_app_whose_focused_window_takes_no_input` |
| X101 | S30 | `_NET_ACTIVE_WINDOW` follows focus | Awakened PoE Trade | `x11_windows::the_active_window_follows_focus` |

## Configure and geometry

| ID | Sources | Scenario | Apps | Status |
|---|---|---|---|---|
| X35 | S15, W23 | A request the WM doesn't follow (maximized window resizing, app window moving) still gets a ConfigureNotify of where the window is | | `x11_configure::a_refused_request_is_answered`, `x11_configure::a_refused_move_is_answered` |
| X36 | W24 | A request changes only the fields it sets | | `x11_configure::a_request_changes_only_what_it_asks` |
| X37 | W25 | Stacking-only requests are answered; odd stack modes do no harm | Wine | `x11_configure::stacking_requests_are_answered` |
| X38 | S15, W27 | The window is told its real root position each time it is placed | Firefox, tooltips | `x11_configure::the_window_is_told_where_it_is` |
| X39 | W10 | A size set before mapping is kept | | `x11_configure::a_size_asked_for_before_mapping_is_kept` |
| X40 | W15 | Maximized before mapping: told its final size before it shows | | `x11_configure::maximized_before_mapping_gets_its_size_first` (Helix's window manager; Smithay's maps the window in its frame first) |
| X41 | W34 | Size hints: the minimum is kept; a base size stands in for a missing minimum | | `x11_configure::the_minimum_size_is_kept`, `x11_configure::the_base_size_stands_in_for_a_minimum` (fixed in #211) |
| X42 | S17 | `_GTK_FRAME_EXTENTS`: a maximized window's shadow goes off screen | GTK CSD apps | `x11_configure::a_maximized_window_puts_its_shadow_off_screen` |
| X43 | S18, W86 | `_NET_WM_SYNC_REQUEST` before each resize for apps that sync | | `x11_configure::resizes_are_announced_to_apps_that_sync` (Helix's window manager), `x11_configure::an_app_that_never_answers_a_sync_request_still_resizes` |
| X44 | W88 | `_NET_WM_MOVERESIZE`: move follows the pointer; unknown directions ignored | Steam, Chromium, Electron | `x11_configure::an_app_moves_its_window_from_its_own_title_bar` |
| X73 | S19 | The frame's visual dropped the client's alpha | | not applicable: no frame windows |
| X75 | S16 | Coordinates rounded wrongly with an Xwayland client scale | `x11_configure::coordinates_at_an_xwayland_scale_of_two` (Helix's Xwayland scale is always whole, see `xwayland_scale`, so the fractional rounding of S16 can't happen; this checks both directions at scale 2) |
| X94 | W30 | Input shape: clicks outside it go through | shaped windows, OSDs | `x11_focus::the_pointer_outside_an_input_shape_goes_through` (fixed in #208) |
| X95 | W28 | Windows at negative layout coordinates | | not testable headlessly: the headless session has one monitor at 0,0 |
| X98 | W43 | `_MOTIF_WM_HINTS` without decorations: no title bar from Helix | Steam, Chromium | `x11_props::motif_hints_without_decorations_get_no_title_bar`; a truncated property: `x11_props` garbage tests |

## Properties

| ID | Sources | Scenario | Apps | Status |
|---|---|---|---|---|
| X45 | S20, W33, W35, W44 | Malformed properties (short, wrong format or type, unterminated, huge, overflowing icons) before and after mapping | | `x11_props::malformed_properties_do_no_harm` |
| X46 | W36 | Transient-for cycles | | `x11_props::transient_cycles_do_no_harm` |
| X47 | S22 | Changing the window type leaves `_NET_WM_STATE` alone | | `x11_props::a_window_type_change_leaves_the_state_alone` |
| X54 | W39, W40 | Title: `_NET_WM_NAME`, falling back to `WM_NAME`; `WM_NAME` is Latin-1 | Firefox | `x11_shell::titles_fall_back_and_decode` |
| X55 | S21, W41 | `WM_CLASS` instance then class; short or unterminated values | | `x11_shell::the_class_names_the_app` |
| X56 | W8 | Properties set before mapping are there at first show | | `x11_shell::properties_set_before_mapping_show_at_once` |
| X87 | S23, W42 | `_NET_WM_WINDOW_OPACITY` | xwaylandvideobridge | not applicable: Helix doesn't apply X11 window opacity |
| X88 | W29 | Struts and `_NET_WORKAREA` | X11 panels; Qt, Java and Wine read the work area | X11 struts: not applicable (Helix doesn't honor them). `_NET_WORKAREA` published from Helix's own panels (#227): `x11_ewmh::the_work_area_follows_the_panels`, `x11_ewmh::the_desktop_is_described_to_apps` |
| X89 | W46 | Startup notification (`_NET_STARTUP_ID`, `_NET_STARTUP_INFO`) | | not applicable: Helix doesn't read X11 startup notification |

## `_NET_WM_STATE` and the WM's identity

| ID | Sources | Scenario | Apps | Status |
|---|---|---|---|---|
| X48 | W31 | `_NET_SUPPORTING_WM_CHECK` and `_NET_WM_NAME`: Java decides whether the WM reparents by them | Java Swing, JetBrains, GeoGebra | `x11_props::the_window_manager_identifies_itself` |
| X49 | W85 | `WM_S0` and `_NET_WM_CM_S0` are owned (apps use ARGB visuals only with a compositing manager) | Firefox, GTK CSD | `x11_props::the_manager_selections_are_owned` |
| X50 | S26 | `_NET_SUPPORTED` lists the states and requests handled | Awakened PoE Trade | `x11_props::net_supported_lists_what_is_handled` |
| X51 | S26, W59 | Two states in one message, either order; toggle; wrong format ignored | | `x11_props::state_messages_with_two_states` |
| X52 | S24 | After mapping, writing `_NET_WM_STATE` directly changes nothing | | `x11_props::writing_the_state_directly_after_mapping_changes_nothing` |
| X102 | S24, W15 | States set before mapping are honored | games | `x11_windows::fullscreen_asked_for_before_mapping`, `x11_windows::modal_dialogs_take_their_parents_focus` |
| X103 | W60 | Fullscreen granted shows in `_NET_WM_STATE` and the size | evince | `x11_windows::a_fullscreen_or_maximized_window_keeps_its_size` |
| X92 | W22 | Window types are hints; Chromium's combobox popups are typed NOTIFICATION | Chromium, Firefox | `x11_windows::splash_screens_notifications_and_docks`; NOTIFICATION windows get no focus by Helix's choice (#168) |

## Stacking

| ID | Sources | Scenario | Apps | Status |
|---|---|---|---|---|
| X58 | S36, W16, W56, W57 | The X stack follows what is on screen, and `_NET_CLIENT_LIST_STACKING` matches it, each window once | Slack, Discord, Chromium, Electron | `x11_stacking::the_x_stack_follows_focus`, `x11_stacking::the_x_stack_follows_raise_and_lower`, `x11_stacking::the_stacking_list_is_the_x_stack` (fixed in #213) |
| X59 | W20 | Raising an app window keeps it under menus and tooltips | | `x11_stacking::raised_windows_stay_under_menus` (fixed in #214) |
| X60 | W37 | A parentless popup over the app's fullscreen window is above it | Chromium | `x11_stacking::a_window_over_a_fullscreen_window_is_above_it` |
| X74 | S37 | Frames' MapNotify listed windows twice | | not applicable: no frame windows (duplicates are X3, X58) |

## Closing

| ID | Sources | Scenario | Apps | Status |
|---|---|---|---|---|
| X57 | W64, W38 | Close asks windows listing `WM_DELETE_WINDOW`; others, or ones that stopped listing it, are disconnected | xterm | `x11_shell::closing_asks_a_window_that_takes_part`, `x11_shell::closing_disconnects_an_app_that_does_not_take_part` (fixed in #209) |

## Clipboard

| ID | Sources | Scenario | Apps | Status |
|---|---|---|---|---|
| X61 | S42, W67 | A large Wayland copy reaches an X11 app whole, incrementally, ending with an empty chunk; also to an InputOnly requestor window | Discord, GIMP, Element | `x11_selection::a_large_wayland_copy_reaches_x11_whole`, `x11_selection::a_large_wayland_copy_reaches_an_input_only_requestor` (fixed in #193) |
| X62 | S43 | One answer per request, no stray refusal after | | `x11_selection::each_request_gets_one_answer`, `x11_clipboard::an_x11_app_pastes_more_than_once` |
| X63 | S44 | A Wayland app pastes an X11 copy again and again | | `x11_selection::a_wayland_app_pastes_an_x11_copy_again_and_again` |
| X64 | W67 | A large X11 copy pasted twice in a row | | `x11_selection::a_large_x11_copy_pastes_whole_twice` |
| X65 | S41 | A paste given up halfway doesn't stall the X11 owner | | `x11_selection::a_paste_given_up_halfway_does_not_stall_the_owner` (intermittent before #218) |
| X66 | W66 | Two pastes of an X11 copy at once | | `x11_selection::two_pastes_at_once` |
| X67 | W69 | An app asking twice and reading neither doesn't hold up others | fcitx, Slack, Telegram | `x11_selection::an_app_abandoning_its_requests_does_not_hold_up_others` |
| X68 | W70 | A requestor gone halfway through a large paste | fcitx | `x11_selection::a_requestor_gone_halfway_does_not_hold_up_others` |
| X69 | W71 | Unknown types are refused at once, every time | | `x11_selection::unknown_types_are_refused_at_once` |
| X70 | W72 | Wayland text under every X11 text target (UTF8_STRING, STRING, TEXT) | xclip | `x11_selection::wayland_text_under_every_x11_text_target` (fixed in #216) |
| X71 | W75 | A hung X11 owner doesn't break the clipboard for good | copyq, fcitx | `x11_selection::a_hung_owner_does_not_break_the_clipboard_for_good` |
| X104 | W73 | Every type an X11 app offers reaches Wayland | Chromium → LibreOffice | `x11_clipboard::an_x11_image_pastes_into_a_wayland_app` |
| X105 | S40 | The INCR size hint is not data | GTK on X11 | `x11_clipboard::a_large_x11_image_pastes_whole` |
| X91 | W74 | X11 apps refused the clipboard while no X11 window has focus | | not applicable: Helix serves every request, as GNOME does (#172) |
| X96 | W65 | CLIPBOARD and PRIMARY changed at once keep their own types | `x11_selection::clipboard_and_primary_taken_at_once_keep_their_own_types` |
| X97 | W68 | Ownership flipping fast between an X11 app and the WM: no self-copy loop | Emacs | `x11_selection::clipboard_ownership_flipping_fast_settles` (fixed in #197), `x11_selection::the_clipboard_recovers_after_ownership_flipped_fast` |

## Drag and drop

| ID | Sources | Scenario | Apps | Status |
|---|---|---|---|---|
| X114 | W76 | Wayland onto X11: XdndEnter, Position, Drop, data | GTK, Firefox | `x11_dnd::a_wayland_app_drags_onto_an_x11_window` |
| X107 | S45 | X11 drag whose messages come from another window than the selection owner | Chromium, Electron | `x11_dnd::a_drag_whose_messages_come_from_another_window` |
| X108 | S46 | XdndFinished names the target window | `x11_dnd::the_drop_window_over_wayland_is_named_and_finishes_in_its_own_name` |
| X109 | S47 | The WM's drop proxy has a WM_NAME | Chromium, Electron | `x11_dnd::the_drop_window_over_wayland_is_named_and_finishes_in_its_own_name` |
| X110 | S48 | `XdndProxy` on an X11 drop target | `x11_dnd::a_drop_target_behind_an_xdnd_proxy_gets_the_drag_there`, `x11_dnd::a_broken_xdnd_proxy_is_not_followed` |
| X111 | S49 | The X11 drag source destroyed mid-drag: the drag ends | `x11_dnd::a_drag_whose_source_goes_away_ends` (fixed in #206) |
| X112 | S50 | An override-redirect drag icon under the pointer isn't the target | Chromium, Electron | `x11_dnd::a_drag_icon_under_the_pointer_is_not_the_target` |
| X113 | S51 | Wayland drag over one X11 window, then another: the drop lands on the second | `x11_dnd::a_wayland_drag_goes_from_one_x11_window_to_another` |
| X115 | W77 | XdndFinished with performed false ends the drag; a forged one is ignored | `x11_dnd::a_drop_the_target_refuses_ends_the_drag_and_a_forged_finish_does_not` |
| X116 | W78 | The X11 target asks for the data a while after the drop | `x11_dnd::a_drop_fetched_late_is_still_served` |
| X117 | W79 | The X11 target window destroyed mid-drag | `x11_dnd::a_drag_whose_x11_target_goes_away_goes_on` (shutting the window manager down mid-drag: not testable headlessly, Xwayland only goes away by crashing) |
| X118 | W80 | Several targets asked for at once during a drag | Chromium | `x11_dnd::several_types_fetched_at_once` (fixed in #207), `x11_dnd::several_types_fetched_one_after_another` |

## Xwayland itself

| ID | Sources | Scenario | Apps | Status |
|---|---|---|---|---|
| X53 | S58 | `_XSETTINGS_SETTINGS` reads back whole (strings padded) | GTK | `x11_props::xsettings_read_back_whole` |
| X83 | S60, W82 | Xwayland gone: windows dropped, started again, X11 apps work | | `x11_windows::xwayland_is_started_again`, `x11_windows::the_window_list_drops_x11_windows_when_xwayland_goes` |
| X84 | S61 | Stale lock and socket of a dead X server: the display reused, the lock 11 bytes | | `x11_windows::the_display_is_released_at_exit`; reuse not testable headlessly (display numbers race with other sessions) |
| X86 | S59, W81 | A root cursor from the theme | GTK | `x11_windows::x11_windows_show_the_cursor_theme` |
| X79 | S57 | Calling back into the WM from its callbacks deadlocked | | not applicable: Smithay's Rust API, not visible to X11 |
| X80 | W83 | The WM's descriptor closed twice | | not applicable: descriptor ownership inside the compositor, not visible to X11 |
| X81 | W87 | Waiting on X replies blocks the compositor: a busy or stuck Xwayland, or a client holding the server | StarCraft II, ImageMagick `import` | `x11_blocking::a_stuck_xwayland_does_not_stop_the_desktop` (Xwayland stopped with SIGSTOP; Smithay's: known failure), `x11_blocking::a_held_x_server_does_not_stop_the_desktop` (Xwayland keeps answering its window manager during a grab) |
| X82 | W84 | Xwayland started on demand and stopped when idle | | not applicable: Helix starts Xwayland with the session |
| X85 | S56 | Setting the RandR primary output again | | not applicable: Helix doesn't set Xwayland's RandR primary |
