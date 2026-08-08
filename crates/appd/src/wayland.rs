use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, Instant};

use khronos_egl as egl;
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_registry,
    delegate_seat,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_seat, wl_surface},
    Connection, Proxy, QueueHandle,
};
use wayland_egl::WlEglSurface;

use calloop::timer::{TimeoutAction, Timer};
use calloop::EventLoop;
use calloop_wayland_source::WaylandSource;

use crate::desktop;
use crate::fileindex;
use crate::font;
use crate::gitscan;
use crate::icon;
use crate::matcher;
use crate::recents;
use crate::render;
use crate::state;
use crate::watcher::{self, WatchEvent};
use protocol::Command;

const FILES_DEFAULT_LIMIT: usize = 20; // shown / opened when no query active
const FILES_SEED_LIMIT: usize = 30;    // initial xbel+local seed before fileindex completes

const CURRENT_SYSTEM_POLL: Duration = Duration::from_secs(5);
// Periodic re-walk for files + git so the daemon picks up new files and new
// git checkouts without a manual restart. inotify on local home would be
// faster but NFS doesn't support inotify, so we poll at a moderate rate that
// covers both. Daemon is idle between ticks.
const REINDEX_INTERVAL: Duration = Duration::from_secs(300);

const REPEAT_TICK: Duration = Duration::from_millis(16);
const DEFAULT_REPEAT_DELAY: Duration = Duration::from_millis(400);
const DEFAULT_REPEAT_RATE: Duration = Duration::from_millis(33);

const FALLBACK_WIDTH: u32 = 1200;
const FALLBACK_HEIGHT: u32 = 720;

// Preferred size is ~62.5% of the output (5/8) — bigger than half so the
// launcher fills enough of the screen to be readable without going fullscreen.
// The minimums keep it usable on a large screen; the margin caps it so the
// window always fits entirely on a small one, minimums included.
const MIN_WIDTH: i32 = 720;
const MIN_HEIGHT: i32 = 520;
const SCREEN_MARGIN: i32 = 24;

fn size_for_output(logical_w: i32, logical_h: i32) -> (u32, u32) {
    let fit_w = (logical_w - 2 * SCREEN_MARGIN).max(1);
    let fit_h = (logical_h - 2 * SCREEN_MARGIN).max(1);
    let w = ((logical_w * 5) / 8).max(MIN_WIDTH).min(fit_w);
    let h = ((logical_h * 5) / 8).max(MIN_HEIGHT).min(fit_h);
    (w as u32, h as u32)
}

type Egl = egl::DynamicInstance<egl::EGL1_5>;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Mode {
    Launcher,
    Files,
    Git,
}

impl Mode {
    const ALL: [Mode; 3] = [Mode::Launcher, Mode::Files, Mode::Git];

    fn label(self) -> &'static str {
        match self {
            Mode::Launcher => "Launcher",
            Mode::Files => "File search",
            Mode::Git => "Git projects",
        }
    }

    fn next(self) -> Self {
        match self {
            Mode::Launcher => Mode::Files,
            Mode::Files => Mode::Git,
            Mode::Git => Mode::Launcher,
        }
    }

    fn prev(self) -> Self {
        match self {
            Mode::Launcher => Mode::Git,
            Mode::Files => Mode::Launcher,
            Mode::Git => Mode::Files,
        }
    }
}

pub struct ListEntry {
    pub label: String,
    #[allow(dead_code)] // used by upcoming icon resolver
    pub icon_hint: Option<String>,
    pub action: Action,
    pub usage_key: Option<String>,
}

#[derive(Clone)]
struct RepeatState {
    event: KeyEvent,
    next_fire: Instant,
}

pub enum Action {
    Exec(Vec<String>),
    Open(PathBuf),
}

// File-search runs on a worker thread so typing stays smooth even when the
// indexed file set is large. The main thread updates `query` and redraws
// immediately on each keystroke, then dispatches a search request; the
// worker computes hits and sends them back via a calloop channel.
struct SearchRequest {
    gen: u64,
    query: String,
    labels: Arc<Vec<String>>,
}

struct SearchResult {
    gen: u64,
    hits: Vec<matcher::Hit>,
}

fn spawn_search_worker(
    rx: mpsc::Receiver<SearchRequest>,
    tx: calloop::channel::Sender<SearchResult>,
) {
    thread::Builder::new()
        .name("filesearch".into())
        .spawn(move || {
            let mut matcher = matcher::Index::new();
            while let Ok(mut req) = rx.recv() {
                // Drain any newer pending requests — only the latest matters.
                while let Ok(newer) = rx.try_recv() {
                    req = newer;
                }
                let hits = matcher.search(&req.labels[..], |s| s.as_str(), &req.query);
                if tx.send(SearchResult { gen: req.gen, hits }).is_err() {
                    return;
                }
            }
        })
        .ok();
}

pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, queue) = registry_queue_init::<App>(&conn)?;
    let qh = queue.handle();

    let compositor_state = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;

    // EGL display + config + context survive across show/hide cycles.
    // No window-bound surface is created until the first show().
    let egl = unsafe { Egl::load_required()? };
    let display_ptr = conn.backend().display_ptr() as *mut c_void;
    let egl_display = unsafe { egl.get_display(display_ptr) }
        .ok_or("eglGetDisplay returned null")?;
    egl.initialize(egl_display)?;

    let config = egl
        .choose_first_config(
            egl_display,
            &[
                egl::SURFACE_TYPE, egl::WINDOW_BIT,
                egl::RENDERABLE_TYPE, egl::OPENGL_BIT,
                egl::RED_SIZE, 8,
                egl::GREEN_SIZE, 8,
                egl::BLUE_SIZE, 8,
                egl::ALPHA_SIZE, 8,
                egl::NONE,
            ],
        )?
        .ok_or("no matching EGL config")?;

    egl.bind_api(egl::OPENGL_API)?;
    let context = egl.create_context(
        egl_display,
        config,
        None,
        &[
            egl::CONTEXT_MAJOR_VERSION, 3,
            egl::CONTEXT_MINOR_VERSION, 3,
            egl::CONTEXT_OPENGL_PROFILE_MASK, egl::CONTEXT_OPENGL_CORE_PROFILE_BIT,
            egl::NONE,
        ],
    )?;

    let mut event_loop: EventLoop<'static, App> = EventLoop::try_new()?;
    let loop_handle = event_loop.handle();

    WaylandSource::new(conn.clone(), queue)
        .insert(loop_handle.clone())
        .map_err(|e| format!("WaylandSource: {e}"))?;

    let (cmd_tx, cmd_rx) = calloop::channel::channel::<Command>();
    if let Err(e) = crate::socket::spawn(cmd_tx) {
        if crate::socket::already_running(&e) {
            log::info!("{e} — exiting, the running instance owns modkey+p");
            return Ok(());
        }
        return Err(e.into());
    }
    loop_handle
        .insert_source(cmd_rx, |ev, _, app: &mut App| {
            if let calloop::channel::Event::Msg(cmd) = ev {
                app.dispatch(cmd);
            }
        })
        .map_err(|e| format!("insert socket source: {e}"))?;

    let (watch_tx, watch_rx) = calloop::channel::channel::<WatchEvent>();
    watcher::spawn(desktop::search_paths(), watch_tx);
    loop_handle
        .insert_source(watch_rx, |ev, _, app: &mut App| {
            if let calloop::channel::Event::Msg(WatchEvent::Reindex) = ev {
                app.reindex_apps("inotify");
            }
        })
        .map_err(|e| format!("insert watcher source: {e}"))?;

    loop_handle
        .insert_source(
            Timer::from_duration(CURRENT_SYSTEM_POLL),
            |_deadline, _, app: &mut App| {
                app.poll_current_system();
                TimeoutAction::ToDuration(CURRENT_SYSTEM_POLL)
            },
        )
        .map_err(|e| format!("insert timer source: {e}"))?;

    loop_handle
        .insert_source(
            Timer::from_duration(REPEAT_TICK),
            |_deadline, _, app: &mut App| {
                app.tick_repeat();
                TimeoutAction::ToDuration(REPEAT_TICK)
            },
        )
        .map_err(|e| format!("insert repeat timer: {e}"))?;

    let entries = desktop::index().unwrap_or_default();
    log::info!("indexed {} .desktop entries", entries.len());

    let extra_icon_dirs = derive_icon_dirs(&entries);
    log::info!("icon extra dirs from packages: {}", extra_icon_dirs.len());
    let icon_resolver = icon::Resolver::build(extra_icon_dirs);

    let persisted = state::load();
    log::info!("state: {} app-usage records", persisted.app_usage.len());

    let launcher_list = build_launcher_list(&entries, &persisted);
    // Files: seed with the cheap xbel + Downloads/Documents/Desktop walk so
    // the tab shows something immediately. The full home + NFS walk runs on
    // a background thread and replaces this list when it finishes.
    let files_list = build_files_list();
    log::info!("files (seed): {} entries", files_list.len());
    let git_list: Vec<ListEntry> = Vec::new();

    let (file_idx_tx, file_idx_rx) = calloop::channel::channel::<Vec<recents::Recent>>();
    loop_handle
        .insert_source(file_idx_rx, |ev, _, app: &mut App| {
            if let calloop::channel::Event::Msg(recents) = ev {
                app.update_files_from_index(recents);
            }
        })
        .map_err(|e| format!("insert fileindex source: {e}"))?;

    let (git_tx, git_rx) = calloop::channel::channel::<Vec<gitscan::GitProject>>();
    loop_handle
        .insert_source(git_rx, |ev, _, app: &mut App| {
            if let calloop::channel::Event::Msg(projects) = ev {
                app.update_git_from_scan(projects);
            }
        })
        .map_err(|e| format!("insert gitscan source: {e}"))?;

    // Background search worker: keeps file-search typing smooth even on huge
    // indices. Main thread sends (gen, query, labels) and redraws immediately;
    // worker returns hits via the calloop channel below.
    let (search_req_tx, search_req_rx) = mpsc::channel::<SearchRequest>();
    let (search_res_tx, search_res_rx) = calloop::channel::channel::<SearchResult>();
    spawn_search_worker(search_req_rx, search_res_tx);
    loop_handle
        .insert_source(search_res_rx, |ev, _, app: &mut App| {
            if let calloop::channel::Event::Msg(result) = ev {
                app.on_search_result(result);
            }
        })
        .map_err(|e| format!("insert search source: {e}"))?;

    // Initial walks — populate as soon as daemon starts.
    fileindex::spawn(file_idx_tx.clone());
    gitscan::spawn(git_tx.clone());

    // Background re-walks: every REINDEX_INTERVAL kicks off fresh threads.
    // Daemon stays interactive while threads work; latest results overwrite
    // files_list / git_list when each completes.
    loop_handle
        .insert_source(
            Timer::from_duration(REINDEX_INTERVAL),
            move |_deadline, _, _app: &mut App| {
                log::info!("periodic reindex tick");
                fileindex::spawn(file_idx_tx.clone());
                gitscan::spawn(git_tx.clone());
                TimeoutAction::ToDuration(REINDEX_INTERVAL)
            },
        )
        .map_err(|e| format!("insert reindex timer: {e}"))?;

    let font_data = font::load_default()
        .map_err(|e| format!("font: {e}"))?;

    let files_labels = labels_for(&files_list);
    let mut app = App {
        qh: qh.clone(),
        registry: RegistryState::new(&globals),
        output: OutputState::new(&globals, &qh),
        seat: SeatState::new(&globals, &qh),
        keyboard: None,
        modifiers: Modifiers::default(),
        repeat: None,
        repeat_delay: DEFAULT_REPEAT_DELAY,
        repeat_rate: DEFAULT_REPEAT_RATE,
        compositor: compositor_state,
        layer_shell,
        egl_static: EglStatic {
            instance: egl,
            display: egl_display,
            config,
            context,
        },
        gl: None,
        renderer: None,
        font_data,
        exit: false,
        session: None,
        entries,
        last_current_system: read_current_system(),
        query: String::new(),
        matches: Vec::new(),
        selected: 0,
        scroll_offset: 0,
        matcher: matcher::Index::new(),
        mode: Mode::Launcher,
        launcher_list,
        files_list,
        files_labels,
        git_list,
        persisted,
        icon_resolver,
        search_tx: search_req_tx,
        search_gen: 0,
    };
    app.refresh_matches();

    log::info!("daemon ready — waiting for show command on socket");

    while !app.exit {
        event_loop.dispatch(None, &mut app)?;
    }
    Ok(())
}

pub struct App {
    qh: QueueHandle<App>,
    registry: RegistryState,
    output: OutputState,
    seat: SeatState,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    modifiers: Modifiers,
    repeat: Option<RepeatState>,
    repeat_delay: Duration,
    repeat_rate: Duration,
    compositor: CompositorState,
    layer_shell: LayerShell,
    egl_static: EglStatic,
    // gl + renderer are tied to the EGL context, not to a particular surface.
    // They're created lazily on the first show() and reused across every
    // subsequent show/hide cycle, so the icon atlas / glyph cache survive.
    gl: Option<glow::Context>,
    renderer: Option<render::Renderer>,
    font_data: Vec<u8>,
    exit: bool,
    // Some(...) iff the launcher window is currently mapped. Show creates it,
    // hide tears it down — the daemon itself stays running.
    session: Option<UiSession>,
    entries: Vec<desktop::Entry>,
    last_current_system: Option<PathBuf>,
    query: String,
    matches: Vec<matcher::Hit>,
    selected: usize,
    scroll_offset: usize,
    matcher: matcher::Index,
    mode: Mode,
    launcher_list: Vec<ListEntry>,
    files_list: Vec<ListEntry>,
    files_labels: Arc<Vec<String>>,
    git_list: Vec<ListEntry>,
    persisted: state::State,
    icon_resolver: icon::Resolver,
    search_tx: mpsc::Sender<SearchRequest>,
    search_gen: u64,
}

struct UiSession {
    layer: LayerSurface,
    egl_window: Option<WlEglSurface>,
    egl_surface: Option<egl::Surface>,
    width: u32,
    height: u32,
    configured: bool,
    size_locked: bool,
    // The output the surface is currently on, as reported by surface_enter.
    // Sizing follows this output, not whichever output happened to be first.
    output: Option<wl_output::WlOutput>,
    /* Når surfacet ble laget. Kommer ingen configure innen
     * SESSION_CONFIGURE_GRACE er sesjonen tapt og neste show() bygger den
     * på nytt istf å tro at launcheren er oppe. */
    created: std::time::Instant,
}

const SESSION_CONFIGURE_GRACE: std::time::Duration = std::time::Duration::from_millis(700);

struct EglStatic {
    instance: Egl,
    display: egl::Display,
    config: egl::Config,
    context: egl::Context,
}

fn read_current_system() -> Option<PathBuf> {
    std::fs::read_link("/run/current-system").ok()
}

// Detaches stdio to /dev/null so the spawned program isn't tied to the
// daemon's file descriptors (Wayland socket, EGL handles, watcher fd's etc).
// Stdio::null only adds posix_spawn file_actions, so the fast path is kept —
// glibc's posix_spawn uses CLONE_VM/CLONE_VFORK rather than copying the
// daemon's page tables on fork(), which materially shortens cold-launch when
// the daemon's RSS has grown (icon atlas + file index ~50–100MB).
//
// We log the wall-clock time spawn() takes — if app launch feels slow but
// this is sub-millisecond, the latency is in the application's own startup
// (Electron/Java/JIT/GPU init) and not anything the launcher can shorten.
fn spawn_detached(prog: &str, args: &[String]) -> bool {
    use std::process::Stdio;
    let started = std::time::Instant::now();
    let result = std::process::Command::new(prog)
        .args(args)
        // Wrapper injects LD_LIBRARY_PATH for the daemon's own dlopen needs
        // (libEGL/libGL via khronos-egl dynamic, etc). If that propagates to
        // spawned children, the linker searches Nix store paths before the
        // child's own RPATH — fine for native NixOS apps but adds latency
        // (and breaks lib resolution) for apps that ship their own libs
        // (Steam, AppImages, Electron with bundled libs).
        .env_remove("LD_LIBRARY_PATH")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let elapsed = started.elapsed();
    match result {
        Ok(child) => {
            log::info!(
                "spawn {prog} pid={} in {:.2?} (args={:?})",
                child.id(),
                elapsed,
                args
            );
            true
        }
        Err(e) => {
            log::error!("spawn {prog}: {e}");
            false
        }
    }
}

fn try_spawn(prog: &str, args: &[String]) -> bool {
    use std::process::Stdio;
    std::process::Command::new(prog)
        .args(args)
        .env_remove("LD_LIBRARY_PATH")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .is_ok()
}

fn staging_dir() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    base.join("nixly_launcher-files")
}

fn unique_name(name: &str, taken: &mut std::collections::HashSet<String>) -> String {
    if taken.insert(name.to_string()) {
        return name.to_string();
    }
    // Append " (n)" before extension, mirroring file-manager dedup style.
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, Some(e)),
        _ => (name, None),
    };
    for n in 2..u32::MAX {
        let candidate = match ext {
            Some(e) => format!("{stem} ({n}).{e}"),
            None => format!("{stem} ({n})"),
        };
        if taken.insert(candidate.clone()) {
            return candidate;
        }
    }
    name.to_string()
}

// Keys that shouldn't auto-repeat: each press should be a discrete action
// (open, hide, switch tab) — holding them is rarely intentional and produces
// jarring behavior.
fn is_repeatable(keysym: Keysym) -> bool {
    !matches!(
        keysym,
        Keysym::Escape
            | Keysym::Return
            | Keysym::KP_Enter
            | Keysym::Tab
            | Keysym::ISO_Left_Tab
    )
}

// Strip trailing whitespace then the last "word" (run of non-whitespace).
// Mirrors Ctrl+W in readline / vim insert mode.
fn pop_word(s: &mut String) {
    while s.chars().last().map_or(false, |c| c.is_whitespace()) {
        s.pop();
    }
    while s.chars().last().map_or(false, |c| !c.is_whitespace()) {
        s.pop();
    }
}

// On NixOS, app icons typically live next to the .desktop file inside the
// owning package (`/nix/store/<hash>-pkg/share/{icons,pixmaps}/...`), not in
// the system-wide aggregated theme dir. canonicalize follows the per-user
// profile symlinks back to the real package, and we register that package's
// icon and pixmap dirs so the resolver finds them.
fn derive_icon_dirs(entries: &[desktop::Entry]) -> Vec<PathBuf> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for e in entries {
        let real = std::fs::canonicalize(&e.path).unwrap_or_else(|_| e.path.clone());
        let Some(applications) = real.parent() else { continue };
        let Some(share) = applications.parent() else { continue };
        for sub in ["icons", "pixmaps"] {
            let p = share.join(sub);
            if seen.insert(p.clone()) {
                out.push(p);
            }
        }
    }
    out
}

fn build_launcher_list(entries: &[desktop::Entry], st: &state::State) -> Vec<ListEntry> {
    let mut list: Vec<ListEntry> = entries
        .iter()
        .filter(|e| !e.exec.is_empty())
        .map(|e| ListEntry {
            label: e.name.clone(),
            icon_hint: e.icon.clone(),
            usage_key: e.exec.first().cloned(),
            action: Action::Exec(e.exec.clone()),
        })
        .collect();
    sort_by_usage(&mut list, st);
    list
}

fn sort_by_usage(list: &mut [ListEntry], st: &state::State) {
    list.sort_by(|a, b| {
        let ua = a
            .usage_key
            .as_deref()
            .and_then(|k| st.app_usage.get(k))
            .copied()
            .unwrap_or_default();
        let ub = b
            .usage_key
            .as_deref()
            .and_then(|k| st.app_usage.get(k))
            .copied()
            .unwrap_or_default();
        ub.last_used
            .cmp(&ua.last_used)
            .then(ub.count.cmp(&ua.count))
            .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
    });
}

fn build_files_list() -> Vec<ListEntry> {
    files_list_from_recents(recents::collect(FILES_SEED_LIMIT))
}

fn files_list_from_recents(recents: Vec<recents::Recent>) -> Vec<ListEntry> {
    recents
        .into_iter()
        // The index walker already skips hidden paths; this also catches xbel
        // recents pointing outside home or into dot-dirs.
        .filter(|r| is_visible_home_file(&r.path))
        .map(|r| ListEntry {
            label: file_label(&r.path),
            icon_hint: Some(icon::DEFAULT_FILE.to_string()),
            usage_key: None,
            action: Action::Open(r.path),
        })
        .collect()
}

// True for files under $HOME whose relative path has no hidden (dot-prefixed)
// component — filters the no-query "recent documents" view down to things the
// user actually edited, not app state churning under ~/.config etc.
fn is_visible_home_file(path: &Path) -> bool {
    let Some(home) = std::env::var_os("HOME") else { return false };
    let Ok(rel) = path.strip_prefix(&home) else { return false };
    !rel.components().any(|c| match c {
        std::path::Component::Normal(s) => s.to_string_lossy().starts_with('.'),
        _ => false,
    })
}

fn file_label(p: &Path) -> String {
    p.file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| p.to_string_lossy().into_owned())
}

fn labels_for(list: &[ListEntry]) -> Arc<Vec<String>> {
    Arc::new(list.iter().map(|e| e.label.clone()).collect())
}

fn git_list_from_projects(projects: Vec<gitscan::GitProject>) -> Vec<ListEntry> {
    projects
        .into_iter()
        .map(|p| ListEntry {
            label: file_label(&p.path),
            icon_hint: Some(icon::DEFAULT_DIR.to_string()),
            usage_key: None,
            action: Action::Open(p.path),
        })
        .collect()
}

impl App {
    fn update_files_from_index(&mut self, recents: Vec<recents::Recent>) {
        let count = recents.len();
        self.files_list = files_list_from_recents(recents);
        self.files_labels = labels_for(&self.files_list);
        log::info!("files index: {count} entries");
        if self.mode == Mode::Files {
            self.refresh_matches();
            if self.session.is_some() {
                self.draw();
            }
        }
    }

    fn update_git_from_scan(&mut self, projects: Vec<gitscan::GitProject>) {
        let count = projects.len();
        self.git_list = git_list_from_projects(projects);
        log::info!("git index: {count} entries");
        if self.mode == Mode::Git {
            self.refresh_matches();
            if self.session.is_some() {
                self.draw();
            }
        }
    }

    fn reindex_apps(&mut self, source: &str) {
        match desktop::index() {
            Ok(entries) => {
                log::info!("reindex ({source}): {} entries", entries.len());
                self.entries = entries;
                self.launcher_list = build_launcher_list(&self.entries, &self.persisted);
                self.refresh_matches();
            }
            Err(e) => log::error!("reindex ({source}): {e}"),
        }
    }

    fn refresh_matches(&mut self) {
        // Bump on every call so any in-flight async result for an older
        // query/list is dropped when it lands.
        self.search_gen = self.search_gen.wrapping_add(1);

        // Files mode with a non-empty query → run the search on the worker
        // thread. We leave self.matches as-is (stale) so the user keeps seeing
        // the previous results until the new ones land — typing stays smooth.
        if self.mode == Mode::Files && !self.query.is_empty() {
            let req = SearchRequest {
                gen: self.search_gen,
                query: self.query.clone(),
                labels: Arc::clone(&self.files_labels),
            };
            let _ = self.search_tx.send(req);
            return;
        }

        let list: &[ListEntry] = match self.mode {
            Mode::Launcher => &self.launcher_list,
            Mode::Files => &self.files_list,
            Mode::Git => &self.git_list,
        };
        self.matches = self.matcher.search(list, |e| e.label.as_str(), &self.query);
        // File search: with no query, limit to the 20 most recently edited.
        // With a query, all fuzzy matches are kept so Enter (→ dolphin staging)
        // collects every result. The list itself is visible-home-only — both
        // the index walker and the xbel seed filter hidden/outside-home paths.
        if self.mode == Mode::Files && self.query.is_empty() {
            self.matches.truncate(FILES_DEFAULT_LIMIT);
        }
        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
        self.scroll_offset = 0;
        self.ensure_visible();
    }

    fn on_search_result(&mut self, result: SearchResult) {
        if result.gen != self.search_gen {
            return;
        }
        if self.mode != Mode::Files || self.query.is_empty() {
            return;
        }
        self.matches = result.hits;
        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
        self.scroll_offset = 0;
        self.ensure_visible();
        if self.session.is_some() {
            self.draw();
        }
    }

    fn ensure_visible(&mut self) {
        let height = self
            .session
            .as_ref()
            .map(|s| s.height)
            .unwrap_or(FALLBACK_HEIGHT);
        let rows = visible_rows_for(height);
        if self.matches.is_empty() {
            self.scroll_offset = 0;
            return;
        }
        if self.selected < self.scroll_offset {
            self.scroll_offset = self.selected;
        } else if self.selected >= self.scroll_offset + rows {
            self.scroll_offset = self.selected + 1 - rows;
        }
    }

    fn cycle_mode(&mut self, forward: bool) {
        self.mode = if forward { self.mode.next() } else { self.mode.prev() };
        self.query.clear();
        self.selected = 0;
        // Don't rebuild files_list on tab switch — the background index owns
        // it now. The tab will show whatever the latest index produced.
        self.refresh_matches();
    }

    fn select_prev(&mut self) {
        if self.selected > 0 {
            self.selected -= 1;
            self.ensure_visible();
        }
    }

    fn select_next(&mut self) {
        if self.selected + 1 < self.matches.len() {
            self.selected += 1;
            self.ensure_visible();
        }
    }

    fn tick_repeat(&mut self) {
        let now = Instant::now();
        let Some(state) = &self.repeat else { return };
        if now < state.next_fire {
            return;
        }
        let event = state.event.clone();
        if let Some(s) = self.repeat.as_mut() {
            s.next_fire = now + self.repeat_rate;
        }
        self.handle_key(event);
    }

    fn arm_repeat(&mut self, event: KeyEvent) {
        if !is_repeatable(event.keysym) {
            self.repeat = None;
            return;
        }
        self.repeat = Some(RepeatState {
            event,
            next_fire: Instant::now() + self.repeat_delay,
        });
    }

    fn disarm_repeat(&mut self, keysym: Keysym) {
        if let Some(state) = &self.repeat {
            if state.event.keysym == keysym {
                self.repeat = None;
            }
        }
    }

    fn handle_key(&mut self, event: KeyEvent) {
        let ctrl = self.modifiers.ctrl;
        let logo = self.modifiers.logo;
        let shift = self.modifiers.shift;

        match event.keysym {
            Keysym::Escape => {
                self.hide();
                return;
            }
            Keysym::Return | Keysym::KP_Enter => {
                self.execute_selected();
                self.hide();
                return;
            }

            // ── Tab cycling ─────────────────────────────────────────────────
            Keysym::Tab => self.cycle_mode(true),
            Keysym::ISO_Left_Tab => self.cycle_mode(false),
            Keysym::Right => self.cycle_mode(true),
            Keysym::Left => self.cycle_mode(false),
            Keysym::p if logo => self.cycle_mode(!shift),
            Keysym::p if ctrl => self.cycle_mode(true),
            Keysym::n if ctrl => self.cycle_mode(false),

            // ── List nav ────────────────────────────────────────────────────
            Keysym::Up => self.select_prev(),
            Keysym::Down => self.select_next(),

            Keysym::BackSpace => {
                self.query.pop();
            }
            Keysym::u if ctrl => {
                self.query.clear();
            }
            Keysym::w if ctrl => {
                pop_word(&mut self.query);
            }

            _ => {
                // Don't echo unhandled Ctrl combos into the query.
                if ctrl {
                    return;
                }
                if let Some(s) = event.utf8.as_deref() {
                    let mut pushed = false;
                    for c in s.chars() {
                        if !c.is_control() {
                            self.query.push(c);
                            pushed = true;
                        }
                    }
                    if !pushed {
                        return;
                    }
                } else {
                    return;
                }
            }
        }
        self.refresh_matches();
        if self.session.is_some() {
            self.draw();
        }
    }

    fn execute_selected(&mut self) {
        match self.mode {
            Mode::Launcher => self.run_launcher_action(),
            Mode::Git => self.run_git_action(),
            Mode::Files => self.show_files_in_dolphin(),
        }
    }

    fn run_launcher_action(&mut self) {
        let Some(hit) = self.matches.get(self.selected).copied() else {
            return;
        };
        let Some(entry) = self.launcher_list.get(hit.idx) else {
            return;
        };
        let usage_key = entry.usage_key.clone();
        if let Action::Exec(cmd) = &entry.action {
            if let Some((prog, args)) = cmd.split_first() {
                spawn_detached(prog, args);
            }
        }
        if let Some(key) = usage_key {
            let usage = self.persisted.app_usage.entry(key).or_default();
            usage.count += 1;
            usage.last_used = state::now_unix();
            sort_by_usage(&mut self.launcher_list, &self.persisted);
            state::save(&self.persisted);
        }
    }

    fn run_git_action(&self) {
        let Some(hit) = self.matches.get(self.selected).copied() else {
            return;
        };
        let Some(entry) = self.git_list.get(hit.idx) else {
            return;
        };
        if let Action::Open(path) = &entry.action {
            spawn_detached(
                "alacritty",
                &[
                    "--working-directory".to_string(),
                    path.to_string_lossy().into_owned(),
                ],
            );
        }
    }

    // Materialises every visible file-search match as a symlink into a stable
    // per-session staging dir under XDG_RUNTIME_DIR, then opens that dir in
    // Dolphin so the user can browse / drag / select multiple results at once.
    // Each Enter clears the dir and repopulates so it always reflects the
    // current query.
    fn show_files_in_dolphin(&self) {
        let stage = staging_dir();
        // Always start clean — never carry over old symlinks from a previous
        // search. remove_dir_all + create_dir_all guarantees a fresh dir.
        let _ = std::fs::remove_dir_all(&stage);
        if let Err(e) = std::fs::create_dir_all(&stage) {
            log::error!("staging dir {}: {e}", stage.display());
            return;
        }

        // Symlink names get a zero-padded "NN - " prefix so Dolphin's default
        // alphabetical sort matches the order the user saw in the launcher
        // list (best match first when searching, mtime-desc otherwise).
        let total = self.matches.len();
        let pad = total.to_string().len().max(2);

        let mut taken: std::collections::HashSet<String> =
            std::collections::HashSet::new();
        let mut linked = 0usize;
        for (rank, hit) in self.matches.iter().enumerate() {
            let Some(entry) = self.files_list.get(hit.idx) else { continue };
            let Action::Open(path) = &entry.action else { continue };
            let Some(stem) = path.file_name().and_then(|s| s.to_str()) else { continue };
            let prefixed = format!("{:0>pad$} - {stem}", rank + 1, pad = pad);
            let name = unique_name(&prefixed, &mut taken);
            let link = stage.join(&name);
            if let Err(e) = std::os::unix::fs::symlink(path, &link) {
                log::debug!("symlink {} -> {}: {e}", link.display(), path.display());
                continue;
            }
            linked += 1;
        }
        log::info!("files: {linked} symlinks in {}", stage.display());

        let stage_str = stage.to_string_lossy().into_owned();
        // Dolphin reuses its existing process via session DBus, so a 2nd Enter
        // re-opens the staging dir near-instantly (no KDE-frameworks cold start).
        if !try_spawn("dolphin", &[stage_str.clone()]) {
            log::warn!("dolphin unavailable — falling back to xdg-open");
            spawn_detached("xdg-open", &[stage_str]);
        }
    }

    fn poll_current_system(&mut self) {
        let now = read_current_system();
        if now != self.last_current_system {
            self.last_current_system = now;
            self.reindex_apps("nixos-rebuild");
        }
    }

    fn dispatch(&mut self, cmd: Command) {
        log::info!("recv {cmd:?}");
        match cmd {
            Command::Toggle => {
                // Only a CONFIGURED session is one the user can see. Anything
                // else goes to show(), which rebuilds a lost surface — else
                // modkey+p would page through an invisible launcher.
                if self.session.as_ref().is_some_and(|s| s.configured) {
                    // Modkey+p is a compositor bind, so the keypress never
                    // reaches our keyboard handler — the toggle command is how
                    // it lands while the launcher is open. Step pages instead.
                    self.cycle_mode(true);
                    self.draw();
                } else {
                    self.show();
                }
            }
            Command::Show => self.show(),
            Command::Hide => self.hide(),
            Command::Quit => self.exit = true,
        }
    }

    // Build a fresh layer surface each time the launcher is shown. The
    // surface is destroyed on hide() — this avoids the wlr-layer-shell
    // re-map quirks (Hyprland in particular can leave an unmapped surface in
    // an awkward state). The EGL display / context survive across cycles so
    // the GL renderer + atlases are reused.
    fn show(&mut self) {
        if let Some(sess) = self.session.as_ref() {
            if sess.configured {
                // Already showing — just redraw so the newest match list lands.
                self.draw();
                return;
            }
            // A session that never got configured is a session the user
            // cannot see: the compositor dropped the layer surface, or the
            // configure was lost. Every later show() would early-return on
            // it and modkey+p would look dead until appd restarts. Rebuild.
            if sess.created.elapsed() < SESSION_CONFIGURE_GRACE {
                return;
            }
            log::warn!("session never configured — rebuilding layer surface");
            self.destroy_session();
        }

        let surface = self.compositor.create_surface(&self.qh);
        let layer = self.layer_shell.create_layer_surface(
            &self.qh,
            surface,
            Layer::Overlay,
            Some("nixly_launcher"),
            None,
        );

        // Provisional size from the smallest known output so the very first
        // frame — drawn before we know which output the compositor puts us on
        // — can never be larger than the screen it lands on. The real size
        // comes from CompositorHandler::surface_enter, which tells us the
        // actual output.
        let (mut width, mut height, mut size_locked) =
            (FALLBACK_WIDTH, FALLBACK_HEIGHT, false);
        for output in self.output.outputs() {
            let Some(info) = self.output.info(&output) else { continue };
            let Some((lw, lh)) = info.logical_size else { continue };
            let (w, h) = size_for_output(lw, lh);
            if !size_locked || w * h < width * height {
                width = w;
                height = h;
            }
            size_locked = true;
        }

        layer.set_anchor(Anchor::empty()); // empty == centered
        layer.set_size(width, height);
        layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
        layer.commit();

        self.session = Some(UiSession {
            layer,
            egl_window: None,
            egl_surface: None,
            width,
            height,
            configured: false,
            size_locked,
            output: None,
            created: std::time::Instant::now(),
        });
        log::info!("show: layer surface created {width}x{height}");
    }

    // Resize the live layer surface to fit `output`. Called whenever we learn
    // which output the launcher is on (surface_enter) or that output's mode
    // changed, so the window always fits the screen it is actually shown on.
    fn resize_to_output(&mut self, output: &wl_output::WlOutput) {
        let Some(info) = self.output.info(output) else { return };
        let Some((lw, lh)) = info.logical_size else { return };
        let (w, h) = size_for_output(lw, lh);
        let Some(sess) = self.session.as_mut() else { return };
        if sess.size_locked && sess.width == w && sess.height == h {
            return;
        }
        sess.layer.set_size(w, h);
        sess.layer.commit();
        sess.size_locked = true;
        log::info!("layer size set to {w}x{h} (output {lw}x{lh})");
    }

    fn hide(&mut self) {
        self.repeat = None;
        // Reset transient UI state so the next show starts at a clean prompt.
        self.query.clear();
        self.selected = 0;
        self.scroll_offset = 0;
        self.mode = Mode::Launcher;
        self.refresh_matches();
        self.destroy_session();
    }

    fn destroy_session(&mut self) {
        let Some(mut sess) = self.session.take() else { return };
        // Order: detach context → destroy egl::Surface (raw handle) → drop
        // WlEglSurface (RAII, frees wl_egl_window) → LayerSurface drops
        // (sends layer_surface.destroy + wl_surface.destroy to compositor).
        let _ = self.egl_static.instance.make_current(
            self.egl_static.display,
            None,
            None,
            None,
        );
        if let Some(egl_surface) = sess.egl_surface.take() {
            let _ = self
                .egl_static
                .instance
                .destroy_surface(self.egl_static.display, egl_surface);
        }
        drop(sess.egl_window.take());
        // sess (containing layer) drops here.
        log::info!("hide: layer surface destroyed");
    }

    fn ensure_egl_window(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let Some(sess) = self.session.as_mut() else { return Ok(()); };
        if sess.egl_window.is_some() {
            return Ok(());
        }
        let surface_id = sess.layer.wl_surface().id();
        let window = WlEglSurface::new(surface_id, sess.width as i32, sess.height as i32)?;
        let egl_surface = unsafe {
            self.egl_static.instance.create_window_surface(
                self.egl_static.display,
                self.egl_static.config,
                window.ptr() as _,
                None,
            )
        }?;
        self.egl_static.instance.make_current(
            self.egl_static.display,
            Some(egl_surface),
            Some(egl_surface),
            Some(self.egl_static.context),
        )?;
        // First show after daemon start: build the GL context + renderer.
        // Subsequent shows reuse them — the EGL context survives surface
        // destruction so all GPU resources (atlases, vertex buffers) stay.
        if self.gl.is_none() {
            let instance_ptr: *const Egl = &self.egl_static.instance;
            let gl = unsafe {
                glow::Context::from_loader_function(|s| {
                    let inst = &*instance_ptr;
                    match inst.get_proc_address(s) {
                        Some(f) => f as *const _,
                        None => std::ptr::null(),
                    }
                })
            };
            let renderer = render::Renderer::new(&gl, &self.font_data)?;
            self.gl = Some(gl);
            self.renderer = Some(renderer);
        }
        sess.egl_window = Some(window);
        sess.egl_surface = Some(egl_surface);
        Ok(())
    }

    fn draw(&mut self) {
        // Bail if no session, or session not yet configured by compositor.
        match self.session.as_ref() {
            Some(s) if s.configured => {}
            _ => return,
        }
        if let Err(e) = self.ensure_egl_window() {
            log::error!("egl window: {e}");
            return;
        }
        let (surface, width, height) = {
            let sess = self.session.as_ref().expect("session present");
            let Some(surface) = sess.egl_surface else { return };
            (surface, sess.width, sess.height)
        };
        if let Err(e) = self.egl_static.instance.make_current(
            self.egl_static.display,
            Some(surface),
            Some(surface),
            Some(self.egl_static.context),
        ) {
            log::error!("make_current: {e}");
            return;
        }

        let gl_opt = self.gl.as_ref();
        let renderer_opt = self.renderer.as_mut();
        let (Some(gl), Some(renderer)) = (gl_opt, renderer_opt) else {
            return;
        };

        let active: &[ListEntry] = match self.mode {
            Mode::Launcher => &self.launcher_list,
            Mode::Files => &self.files_list,
            Mode::Git => &self.git_list,
        };
        let default_key = default_icon_key(self.mode);

        ensure_default_icon(renderer, &mut self.icon_resolver, gl, default_key);

        // Only ensure icons for the rows we'll actually render — saves first-
        // frame decode time when the result set is large.
        let rows = visible_rows_for(height);
        let end = (self.scroll_offset + rows).min(self.matches.len());
        for hit in &self.matches[self.scroll_offset..end] {
            let Some(entry) = active.get(hit.idx) else {
                continue;
            };
            ensure_icon(
                renderer,
                &mut self.icon_resolver,
                gl,
                entry.icon_hint.as_deref(),
            );
        }

        render_ui(
            renderer,
            gl,
            width,
            height,
            self.mode,
            active,
            &self.matches,
            self.selected,
            self.scroll_offset,
            &self.query,
            default_key,
        );

        if let Err(e) = self
            .egl_static
            .instance
            .swap_buffers(self.egl_static.display, surface)
        {
            log::error!("swap_buffers: {e}");
        }
    }
}

const PROCEDURAL_DEFAULT: &str = "__nixly_procedural_default__";

fn default_icon_key(mode: Mode) -> &'static str {
    match mode {
        Mode::Launcher => icon::DEFAULT_APP,
        Mode::Files => icon::DEFAULT_FILE,
        Mode::Git => icon::DEFAULT_DIR,
    }
}

// Side-effecting: makes sure the renderer atlas contains an entry for `name`
// (a real icon-theme name or absolute path). Silently no-ops when already
// cached. Failures fall through to the default icon, which `render_ui`
// resolves via the chained `get_icon` lookups.
fn ensure_icon(
    renderer: &mut render::Renderer,
    resolver: &mut icon::Resolver,
    gl: &glow::Context,
    name: Option<&str>,
) {
    let Some(name) = name else {
        return;
    };
    if name.is_empty() {
        return;
    }
    if renderer.get_icon(name).is_some() {
        return;
    }
    let Some(path) = resolver.resolve(name) else {
        return;
    };
    let Some((rgba, w, h)) = icon::load_rgba(&path) else {
        log::debug!("icon decode {}: failed", path.display());
        return;
    };
    renderer.upload_icon(gl, name, &rgba, w, h);
}

// Two-stage default: try the theme-named default (e.g. application-x-executable)
// first so the launcher inherits the user's icon-theme aesthetic, then fall
// back to the procedural slate.
fn ensure_default_icon(
    renderer: &mut render::Renderer,
    resolver: &mut icon::Resolver,
    gl: &glow::Context,
    default_name: &str,
) {
    if renderer.get_icon(default_name).is_none() {
        if let Some(path) = resolver.resolve(default_name) {
            if let Some((rgba, w, h)) = icon::load_rgba(&path) {
                renderer.upload_icon(gl, default_name, &rgba, w, h);
            }
        }
    }
    if renderer.get_icon(PROCEDURAL_DEFAULT).is_none() {
        let rgba = icon::default_rgba();
        renderer.upload_icon(gl, PROCEDURAL_DEFAULT, &rgba, icon::ICON_SIZE, icon::ICON_SIZE);
    }
}

fn pick_icon(
    renderer: &render::Renderer,
    name: Option<&str>,
    default_name: &str,
) -> Option<render::IconHandle> {
    if let Some(n) = name {
        if let Some(h) = renderer.get_icon(n) {
            return Some(h);
        }
    }
    renderer
        .get_icon(default_name)
        .or_else(|| renderer.get_icon(PROCEDURAL_DEFAULT))
}

const TAB_BAR_H: f32 = 56.0;
const SEARCH_H: f32 = 64.0;
const ROW_H: f32 = 56.0;
const PAD: f32 = 18.0;
const TEXT_SIZE: f32 = 22.0;
const TAB_TEXT_SIZE: f32 = 18.0;
const ICON_BOX: f32 = 42.0;

fn visible_rows_for(height: u32) -> usize {
    let list_h = (height as f32) - TAB_BAR_H - SEARCH_H;
    ((list_h / ROW_H).floor() as i32).max(1) as usize
}

fn render_ui(
    renderer: &mut render::Renderer,
    gl: &glow::Context,
    width: u32,
    height: u32,
    mode: Mode,
    list: &[ListEntry],
    matches: &[matcher::Hit],
    selected: usize,
    scroll_offset: usize,
    query: &str,
    default_icon_name: &str,
) {
    let w = width as f32;
    let h = height as f32;
    renderer.begin(gl, width, height);

    // Outer panel
    renderer.fill_rect(0.0, 0.0, w, h, render::BG);

    // ── Tab bar ─────────────────────────────────────────────────────────────
    let tab_w = w / Mode::ALL.len() as f32;
    for (i, m) in Mode::ALL.iter().enumerate() {
        let x = i as f32 * tab_w;
        let active = *m == mode;
        if active {
            renderer.fill_rect(x, 0.0, tab_w, TAB_BAR_H, render::SEL_BG);
            renderer.fill_rect(x, TAB_BAR_H - 2.0, tab_w, 2.0, render::ACCENT);
        }
        let label = m.label();
        let label_w = approx_text_width(label, TAB_TEXT_SIZE);
        let tx = x + (tab_w - label_w) * 0.5;
        let baseline = TAB_BAR_H * 0.5 + TAB_TEXT_SIZE * 0.36;
        let color = if active { render::FG } else { render::FG_DIM };
        renderer.draw_text(gl, tx, baseline, TAB_TEXT_SIZE, label, color);
    }

    // ── Search bar ──────────────────────────────────────────────────────────
    let sy = TAB_BAR_H;
    renderer.fill_rect(0.0, sy, w, SEARCH_H, render::BG);
    renderer.fill_rect(0.0, sy + SEARCH_H - 1.0, w, 1.0, render::SEL_BG);
    let baseline = sy + SEARCH_H * 0.5 + TEXT_SIZE * 0.36;
    let after_prompt = renderer.draw_text(gl, PAD, baseline, TEXT_SIZE, "›", render::ACCENT);
    let q_x = after_prompt + 8.0;
    if query.is_empty() {
        let placeholder: String = match mode {
            Mode::Launcher => format!("Search {} apps…", list.len()),
            Mode::Files => format!("Search {} files…", list.len()),
            Mode::Git => format!("Search {} git projects…", list.len()),
        };
        renderer.draw_text(gl, q_x, baseline, TEXT_SIZE, &placeholder, render::FG_DIM);
    } else {
        renderer.draw_text(gl, q_x, baseline, TEXT_SIZE, query, render::FG);
    }

    // ── Result list (windowed) ──────────────────────────────────────────────
    let list_top = TAB_BAR_H + SEARCH_H;
    let icon_box = ICON_BOX;
    let visible_rows = (((h - list_top) / ROW_H).floor() as i32).max(1) as usize;
    let end = (scroll_offset + visible_rows).min(matches.len());

    for (slot, i) in (scroll_offset..end).enumerate() {
        let hit = matches[i];
        let row_y = list_top + (slot as f32) * ROW_H;
        if i == selected {
            renderer.fill_rect(0.0, row_y, w, ROW_H, render::SEL_BG);
        }
        let entry = &list[hit.idx];
        let row_baseline = row_y + ROW_H * 0.5 + TEXT_SIZE * 0.36;

        let icon_x = PAD;
        let icon_y = row_y + (ROW_H - icon_box) * 0.5;
        if let Some(handle) = pick_icon(renderer, entry.icon_hint.as_deref(), default_icon_name) {
            renderer.draw_icon(icon_x, icon_y, icon_box, icon_box, handle);
        }

        let label_x = icon_x + icon_box + 12.0;
        let fg = if i == selected { render::FG } else { render::FG_DIM };
        renderer.draw_text(gl, label_x, row_baseline, TEXT_SIZE, &entry.label, fg);
    }

    // Scrollbar — only when there's more list than fits.
    if matches.len() > visible_rows {
        let track_x = w - 4.0;
        let track_w = 3.0;
        let track_h = h - list_top;
        let frac_visible = visible_rows as f32 / matches.len() as f32;
        let thumb_h = (track_h * frac_visible).max(18.0);
        let max_scroll = (matches.len() - visible_rows) as f32;
        let scroll_frac = if max_scroll > 0.0 {
            scroll_offset as f32 / max_scroll
        } else {
            0.0
        };
        let thumb_y = list_top + scroll_frac * (track_h - thumb_h);
        renderer.fill_rect(track_x, list_top, track_w, track_h, [0.18, 0.20, 0.26, 0.6]);
        renderer.fill_rect(track_x, thumb_y, track_w, thumb_h, render::ACCENT);
    }

    if matches.is_empty() {
        let msg = "No matches";
        let mw = approx_text_width(msg, TEXT_SIZE);
        renderer.draw_text(gl, (w - mw) * 0.5, list_top + 36.0, TEXT_SIZE, msg, render::FG_DIM);
    }

    renderer.finish(gl);
}

// Coarse pre-layout estimator: avoids running the glyph cache to measure text
// width. The renderer's actual advance widths are slightly different but the
// difference is below 1px for the sizes we use, and the only place this matters
// is centering tab labels and the "no matches" string.
fn approx_text_width(s: &str, size: f32) -> f32 {
    s.chars()
        .map(|c| {
            let f: f32 = match c {
                'i' | 'l' | 'I' | '|' | '!' | '.' | ',' | ':' | ';' | '\'' => 0.30,
                'f' | 'j' | 'r' | 't' | ' ' => 0.40,
                'M' | 'W' | 'm' | 'w' => 0.85,
                _ => 0.55,
            };
            f
        })
        .sum::<f32>()
        * size
}

impl CompositorHandler for App {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
    }
    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }
    fn frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
        if self.session.is_some() {
            self.draw();
        }
    }
    fn surface_enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        surface: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        // First point at which the compositor tells us which screen the
        // launcher landed on. Size to that screen — the size chosen in show()
        // was only a guess.
        let Some(sess) = self.session.as_mut() else { return };
        if sess.layer.wl_surface() != surface {
            return;
        }
        sess.output = Some(output.clone());
        self.resize_to_output(output);
    }
    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output
    }
    fn new_output(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        output: wl_output::WlOutput,
    ) {
        // Only a fallback: an output appearing while we're up but before any
        // surface_enter told us where we are. Once surface_enter has run, that
        // output owns the size.
        match self.session.as_ref() {
            Some(s) if s.output.is_none() && !s.size_locked => {}
            _ => return,
        }
        self.resize_to_output(&output);
    }
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        // Resolution / scale change on the output we're displayed on.
        let on_this_output = self
            .session
            .as_ref()
            .is_some_and(|s| s.output.as_ref() == Some(&output));
        if on_this_output {
            self.resize_to_output(&output);
        }
    }
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        if let Some(sess) = self.session.as_mut() {
            if sess.output.as_ref() == Some(&output) {
                sess.output = None;
            }
        }
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        // Compositor unmapped our layer surface (e.g. output disconnect). Tear
        // down the session but keep the daemon alive so the next show works.
        self.destroy_session();
    }
    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _serial: u32,
    ) {
        let Some(sess) = self.session.as_mut() else { return };
        let (w, h) = configure.new_size;
        if w > 0 {
            sess.width = w;
        }
        if h > 0 {
            sess.height = h;
        }
        if let Some(window) = sess.egl_window.as_ref() {
            window.resize(sess.width as i32, sess.height as i32, 0, 0);
        }
        sess.configured = true;
        self.draw();
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            match self.seat.get_keyboard(qh, &seat, None) {
                Ok(kbd) => self.keyboard = Some(kbd),
                Err(e) => log::error!("get_keyboard: {e}"),
            }
        }
    }
    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard {
            if let Some(kbd) = self.keyboard.take() {
                kbd.release();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }
    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }
    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        let cloned = event.clone();
        self.handle_key(event);
        self.arm_repeat(cloned);
    }
    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        self.disarm_repeat(event.keysym);
    }
    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        modifiers: Modifiers,
        _: u32,
    ) {
        self.modifiers = modifiers;
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(App);
delegate_output!(App);
delegate_layer!(App);
delegate_seat!(App);
delegate_keyboard!(App);
delegate_registry!(App);
