use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use calloop::channel::Sender;
use notify::{recommended_watcher, Event, RecursiveMode, Watcher};

pub enum WatchEvent {
    Reindex,
}

const DEBOUNCE: Duration = Duration::from_millis(250);
/// Hard floor between two reindexes. DEBOUNCE alone only guarantees a quiet
/// window, so a steady trickle of events spaced just over 250 ms apart drove a
/// full rescan ~4×/s for the whole session (2351 of them in one 10-minute
/// session log) — constant wakeups and log writes underneath running games.
/// The app list is never urgent; once per 5 s is plenty.
const MIN_INTERVAL: Duration = Duration::from_secs(5);

pub fn spawn(paths: Vec<PathBuf>, tx: Sender<WatchEvent>) {
    thread::Builder::new()
        .name("apptoggle-watcher".into())
        .spawn(move || run(paths, tx))
        .expect("spawn watcher");
}

fn run(paths: Vec<PathBuf>, tx: Sender<WatchEvent>) {
    let (raw_tx, raw_rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = match recommended_watcher(raw_tx) {
        Ok(w) => w,
        Err(e) => {
            log::error!("watcher init: {e}");
            return;
        }
    };
    let mut watched = 0usize;
    // XDG_DATA_DIRS repeats dirs that `search_paths()` also lists explicitly
    // (/run/current-system/sw/share, ~/.nix-profile/share, /etc/profiles/...),
    // so the same directory was watched 2-3× and every event arrived that many
    // times.
    let mut seen: HashSet<PathBuf> = HashSet::new();
    for p in &paths {
        let key = std::fs::canonicalize(p).unwrap_or_else(|_| p.clone());
        if !seen.insert(key) {
            continue;
        }
        match watcher.watch(p, RecursiveMode::NonRecursive) {
            Ok(()) => watched += 1,
            // Many candidate dirs don't exist on a given system — debug, not warn.
            Err(e) => log::debug!("watch {}: {e}", p.display()),
        }
    }
    log::info!("watching {watched} app dirs");

    let mut last_sent: Option<Instant> = None;
    loop {
        let first = match raw_rx.recv() {
            Ok(ev) => ev,
            Err(_) => return,
        };
        // Drain until a `DEBOUNCE` window passes with no new events.
        loop {
            match raw_rx.recv_timeout(DEBOUNCE) {
                Ok(_) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
        // Rate limit, then swallow whatever piled up while we waited so the
        // next loop starts from a genuinely new event.
        if let Some(t) = last_sent {
            let since = t.elapsed();
            if since < MIN_INTERVAL {
                thread::sleep(MIN_INTERVAL - since);
                while raw_rx.try_recv().is_ok() {}
            }
        }
        last_sent = Some(Instant::now());
        if let Ok(ev) = &first {
            log::debug!("reindex triggered by {:?}", ev.paths);
        }
        if tx.send(WatchEvent::Reindex).is_err() {
            return;
        }
    }
}
