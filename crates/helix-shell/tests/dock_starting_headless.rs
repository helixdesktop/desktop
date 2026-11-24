//! The dock's "starting" state (helixdesktop/desktop#298; helixdesktop/spec
//! `contracts/wayland/README.md`, "Launch feedback (Helix)", "Dock"), in a
//! headless Helix session (helix-comp with helix-shell, scratch home,
//! private bus): an app launched from the dock is shown starting (its icon
//! dimmed: still without animations, breathing with them) until helix-comp
//! says the launch ended (`helix_window_list_v1.launch_ended`); an app
//! that is not pinned gets an item of its own while it starts; and when
//! helix-comp never says so, the shell's safety timeout clears it.
//!
//! The apps are desktop files of the test's own that never show a window
//! (each touches a file), so their launches end by timing out. Needs
//! helix-comp built next to this test; skipped otherwise.

use std::path::Path;
use std::time::{Duration, Instant};

use helix_test_support::headless::{sibling_binary, HeadlessSession, Options};

const SLOW: &str = "org.example.SlowStart";

/// A desktop file `id` (an app that never shows a window) with a plain
/// white icon, so that dimming it shows.
fn app(home: &Path, id: &str, name: &str) {
    let dir = home.join(".local/share/applications");
    std::fs::create_dir_all(&dir).unwrap();
    let icon = home.join(format!("{id}.png"));
    let file = std::io::BufWriter::new(std::fs::File::create(&icon).unwrap());
    let mut encoder = png::Encoder::new(file, 64, 64);
    encoder.set_color(png::ColorType::Rgba);
    encoder.write_header().unwrap().write_image_data(&[0xFF; 64 * 64 * 4]).unwrap();
    let entry = format!("[Desktop Entry]\nType=Application\nName={name}\nExec=sh -c 'touch \"$HOME/launched-{id}\"'\nIcon={}\nTerminal=false\nStartupNotify=true\n", icon.display());
    std::fs::write(dir.join(format!("{id}.desktop")), entry).unwrap();
}

fn start(settings: &str, env: &[(&str, &str)], prepare: impl FnOnce(&Path)) -> Option<HeadlessSession> {
    let Some(comp) = sibling_binary("helix-comp") else {
        eprintln!("helix-comp is not built next to this test; skipped");
        return None;
    };
    let shell = std::path::PathBuf::from(env!("CARGO_BIN_EXE_helix-shell"));
    let mut env: Vec<(String, String)> = env.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
    env.push(("RUST_LOG".to_owned(), "info,helix_shell::ui::dock=debug,helix_shell::ui::app_grid=debug".to_owned()));
    let session = HeadlessSession::start_prepared(Options { comp: &comp, shell: Some(&shell), size: (1280, 800), settings, env }, prepare)?;
    assert!(session.wait_for_log("namespace=\"helix-panel\"", Duration::from_secs(30)), "the shell's panel:\n{}", session.log());
    Some(session)
}

/// Until the log has a line with all of `parts`: the first such line.
fn log_line(session: &HeadlessSession, parts: &[&str], what: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(line) = session.log().lines().find(|l| parts.iter().all(|p| l.contains(p))) {
            return line.to_owned();
        }
        assert!(Instant::now() < deadline, "{what}: no line with {parts:?}:\n{}", session.log());
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// The last "dock" line's bar (layout coordinates) and item count.
fn dock_bar(session: &HeadlessSession) -> Option<((i32, i32, i32, i32), usize)> {
    let log = session.log();
    let line = log.lines().rev().find(|l| l.contains(" dock") && l.contains("rect=("))?;
    let rect = line.split("rect=(").nth(1)?.split(')').next()?;
    let n: Vec<i32> = rect.split(',').map(|v| v.trim().parse().ok()).collect::<Option<_>>()?;
    let items = line.split("items=").nth(1)?.split_whitespace().next()?.parse().ok()?;
    (n.len() == 4).then(|| ((n[0], n[1], n[2], n[3]), items))
}

/// Until the dock holds `items` items: its bar.
fn dock_with(session: &HeadlessSession, items: usize, what: &str) -> (i32, i32, i32, i32) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some((rect, _)) = dock_bar(session).filter(|(_, n)| *n == items) {
            return rect;
        }
        assert!(Instant::now() < deadline, "{what}: the dock does not hold {items} items ({:?}):\n{}", dock_bar(session), session.log());
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// How bright the middle of the dock's only item is in a fresh snapshot
/// (the mean of R, G and B over 16×16 pixels).
fn item_brightness(session: &HeadlessSession, bar: (i32, i32, i32, i32), keep: Option<&str>) -> f64 {
    let (w, _, rgba) = session.fresh_snapshot();
    if let (Some(name), Some(dir)) = (keep, std::env::var_os("HELIX_DOCK_PNG_DIR")) {
        let _ = std::fs::copy(session.snapshot_path(), Path::new(&dir).join(name));
    }
    let (cx, cy) = (bar.0 + bar.2 / 2, bar.1 + bar.3 / 2);
    let mut sum = 0.0;
    for y in cy - 8..cy + 8 {
        for x in cx - 8..cx + 8 {
            let i = ((y as u32 * w + x as u32) * 4) as usize;
            sum += f64::from(u32::from(rgba[i]) + u32::from(rgba[i + 1]) + u32::from(rgba[i + 2])) / 3.0;
        }
    }
    sum / 256.0
}

/// Launched from the dock without animations: the item is dimmed, still,
/// and busy, until helix-comp's launch_ended (its timeout here) clears it.
#[test]
fn a_dock_item_is_starting_until_the_launch_ends() {
    let settings = format!(
        "[org/gnome/desktop/interface]\nenable-hot-corners=false\nenable-animations=false\n\n[org/gnome/shell]\nfavorite-apps=['{SLOW}.desktop']\n\n[org/gnome/shell/extensions/dash-to-dock]\ndock-fixed=true\nautohide=false\nintellihide=false\nshow-trash=false\nshow-mounts=false\nshow-show-apps-button=false\n"
    );
    // helix-comp gives up after 4 s; the shell would after 5 s.
    let Some(mut session) = start(&settings, &[("HELIX_LAUNCH_TIMEOUT_MS", "4000")], |home| app(home, SLOW, "Slow Start")) else { return };
    let bar = dock_with(&session, 1, "the favorite");
    std::thread::sleep(Duration::from_millis(500));
    let idle = item_brightness(&session, bar, Some("dock-idle.png"));

    let (x, y) = (bar.0 + bar.2 / 2, bar.1 + bar.3 / 2);
    session.input(&format!("click {x} {y}"));
    // Away from the dock: no hover highlight or tooltip.
    session.input("move 640 300");
    let started = Instant::now();
    log_line(&session, &["app starting", SLOW], "launched");
    let shown = log_line(&session, &["dock items starting", SLOW], "the dock shows it starting");
    assert!(shown.contains("pulse=false"), "no animations: still, not breathing: {shown}");
    let dimmed = item_brightness(&session, bar, Some("dock-starting.png"));
    assert!(started.elapsed() < Duration::from_secs(4), "the snapshot came too late");
    assert!(dimmed < idle - 40.0, "the starting item is dimmed: {dimmed:.0}, idle {idle:.0}");

    // helix-comp ends the launch (its timeout): the shell is told, and the
    // item looks as before.
    let ended = log_line(&session, &["app no longer starting", SLOW], "the launch ended");
    assert!(ended.contains("helix-comp's timeout"), "ended by helix-comp's event, not the shell's own timeout: {ended}");
    let waited = started.elapsed();
    assert!(waited >= Duration::from_millis(3500) && waited < Duration::from_millis(4900), "ended by helix-comp's timeout after {waited:?}");
    std::thread::sleep(Duration::from_millis(300));
    let after = item_brightness(&session, bar, None);
    assert!((after - idle).abs() < 10.0, "back as it was: {after:.0}, idle {idle:.0}");
    assert!(session.home().join(format!("launched-{SLOW}")).exists(), "the app ran");
}

/// Open the app grid and click app `id`'s cell.
fn launch_from_grid(session: &mut HeadlessSession, id: &str) {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
    rt.block_on(async {
        let conn = session.bus().connect().await;
        conn.call_method(Some("org.gnome.Shell"), "/org/gnome/Shell", Some("org.gnome.Shell"), "ShowApplications", &()).await.expect("ShowApplications");
    });
    let line = log_line(session, &["app grid shown", id], "the grid");
    let cell = line
        .split_whitespace()
        .map(|w| w.trim_matches(|c| c == '"' || c == '\'').trim_start_matches("cells=").trim_matches('"'))
        .find_map(|w| {
            let (key, rect) = w.split_once('@')?;
            let n: Vec<i32> = rect.split(',').map(|v| v.parse().ok()).collect::<Option<_>>()?;
            (key.contains(id) && n.len() == 4).then(|| (n[0] + n[2] / 2, n[1] + n[3] / 2))
        });
    let Some((x, y)) = cell else { panic!("no cell for {id}: {line}") };
    std::thread::sleep(Duration::from_millis(300));
    session.input(&format!("click {x} {y}"));
}

/// An app that is not pinned, launched from the app grid with animations:
/// an item of its own on the dock while it starts, breathing; helix-comp
/// never ends the launch (its timeout is long), so the shell's safety
/// timeout clears it, and the item goes.
#[test]
fn an_unpinned_app_starts_on_the_dock_until_the_safety_timeout() {
    // The app first on the grid's first page.
    let settings = &format!("[org/gnome/desktop/interface]\nenable-hot-corners=false\nenable-animations=true\n\n[org/gnome/shell]\nfavorite-apps=@as []\napp-picker-layout=[{{'{SLOW}.desktop': <{{'position': <0>}}>}}]\n\n[org/gnome/shell/extensions/dash-to-dock]\ndock-fixed=true\nautohide=false\nintellihide=false\nshow-trash=false\nshow-mounts=false\nshow-show-apps-button=false\n");
    let env = [("HELIX_LAUNCH_TIMEOUT_MS", "60000"), ("HELIX_SHELL_LAUNCH_TIMEOUT_MS", "2500")];
    let Some(mut session) = start(settings, &env, |home| app(home, SLOW, "Slow Start")) else { return };
    dock_with(&session, 0, "nothing pinned or running");
    launch_from_grid(&mut session, SLOW);
    let started = Instant::now();
    log_line(&session, &["app starting", SLOW], "launched");
    let bar = dock_with(&session, 1, "the starting app's own item");
    log_line(&session, &["dock items starting", SLOW, "pulse=true"], "the dock shows it breathing");
    session.input("move 640 300");
    let _ = item_brightness(&session, bar, Some("dock-starting-unpinned.png"));

    let ended = log_line(&session, &["app no longer starting", SLOW], "the shell gave up");
    assert!(ended.contains("safety timeout"), "{ended}");
    assert!(started.elapsed() >= Duration::from_millis(2000), "gave up early: {:?}", started.elapsed());
    dock_with(&session, 0, "the item went with the launch");
}
