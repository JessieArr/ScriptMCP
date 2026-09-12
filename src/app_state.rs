//! Persisted UI/server settings: script folders and which tools are exposed.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// On-disk application settings reloaded on startup.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppState {
    /// Absolute (or user-provided) script source folders.
    pub folders: Vec<PathBuf>,
    /// Absolute script path → whether the tool is exposed over MCP.
    pub enabled: HashMap<String, bool>,
}

impl AppState {
    pub fn config_path() -> PathBuf {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            if !xdg.is_empty() {
                return PathBuf::from(xdg).join("scriptmcp").join("config.json");
            }
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".config")
                .join("scriptmcp")
                .join("config.json");
        }
        PathBuf::from("scriptmcp-config.json")
    }

    pub fn load() -> Self {
        let path = Self::config_path();
        match fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<()> {
        let path = Self::config_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let text =
            serde_json::to_string_pretty(self).context("failed to serialize ScriptMCP config")?;
        fs::write(&path, text + "\n")
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }

    /// Folders to scan: persisted list, or a single CLI/default fallback.
    pub fn folders_or_fallback(&self, fallback: &Path) -> Vec<PathBuf> {
        if self.folders.is_empty() {
            vec![fallback.to_path_buf()]
        } else {
            self.folders.clone()
        }
    }

    pub fn add_folder(&mut self, path: PathBuf) -> bool {
        let normalized = normalize_path(&path);
        if self
            .folders
            .iter()
            .any(|existing| normalize_path(existing) == normalized)
        {
            return false;
        }
        self.folders.push(normalized);
        true
    }

    pub fn remove_folder(&mut self, path: &Path) {
        let normalized = normalize_path(path);
        self.folders
            .retain(|existing| normalize_path(existing) != normalized);
        self.enabled.retain(|tool_path, _| {
            Path::new(tool_path)
                .parent()
                .map(|parent| normalize_path(parent) != normalized)
                .unwrap_or(true)
        });
    }

    pub fn set_enabled(&mut self, path: &Path, enabled: bool) {
        self.enabled
            .insert(normalize_path(path).display().to_string(), enabled);
    }

    pub fn is_enabled(&self, path: &Path, default: bool) -> bool {
        self.enabled
            .get(&normalize_path(path).display().to_string())
            .copied()
            .unwrap_or(default)
    }
}

pub fn normalize_path(path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_path_buf())
    };
    joined.canonicalize().unwrap_or(joined)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_folder_dedupes_normalized_paths() {
        let mut state = AppState::default();
        assert!(state.add_folder(PathBuf::from("/tmp/tools")));
        assert!(!state.add_folder(PathBuf::from("/tmp/tools")));
        assert_eq!(state.folders.len(), 1);
    }
}
