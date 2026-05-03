use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use calloop::channel::Sender;
use notify::{recommended_watcher, Event, RecursiveMode, Watcher};

pub enum WatchEvent {
    Reindex,
}

const DEBOUNCE: Duration = Duration::from_millis(250);

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
    for p in &paths {
        match watcher.watch(p, RecursiveMode::NonRecursive) {
            Ok(()) => watched += 1,
            // Many candidate dirs don't exist on a given system — debug, not warn.
            Err(e) => log::debug!("watch {}: {e}", p.display()),
        }
    }
    log::info!("watching {watched} app dirs");

    loop {
        if raw_rx.recv().is_err() {
            return;
        }
        // Drain until a `DEBOUNCE` window passes with no new events.
        loop {
            match raw_rx.recv_timeout(DEBOUNCE) {
                Ok(_) => continue,
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
        if tx.send(WatchEvent::Reindex).is_err() {
            return;
        }
    }
}
