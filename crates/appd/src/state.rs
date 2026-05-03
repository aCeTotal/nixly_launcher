use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct State {
    #[serde(default)]
    pub app_usage: HashMap<String, AppUsage>,
}

#[derive(Debug, Default, Serialize, Deserialize, Clone, Copy)]
pub struct AppUsage {
    #[serde(default)]
    pub count: u32,
    #[serde(default)]
    pub last_used: u64,
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn state_path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("nixly_launcher").join("state.json")
}

pub fn load() -> State {
    let path = state_path();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return State::default();
    };
    serde_json::from_str(&text).unwrap_or_else(|e| {
        log::warn!("state parse: {e}");
        State::default()
    })
}

pub fn save(state: &State) {
    let path = state_path();
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            log::warn!("state mkdir {}: {e}", dir.display());
            return;
        }
    }
    match serde_json::to_string_pretty(state) {
        Ok(text) => {
            if let Err(e) = std::fs::write(&path, text) {
                log::warn!("state write {}: {e}", path.display());
            }
        }
        Err(e) => log::warn!("state serialize: {e}"),
    }
}
