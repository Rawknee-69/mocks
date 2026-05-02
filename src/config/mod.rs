use directories::ProjectDirs;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use std::time::SystemTime;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
pub enum PushToTalkHotkey {
    #[default]
    CtrlSpace,
    CtrlGrave,
}

impl PushToTalkHotkey {
    pub fn display_name(self) -> &'static str {
        match self {
            Self::CtrlSpace => "Ctrl+Space",
            Self::CtrlGrave => "Ctrl+`",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AppConfig {
    pub push_to_talk_hotkey: PushToTalkHotkey,
    pub autostart_enabled: bool,
}

impl AppConfig {
    pub fn load() -> Self {
        Self::load_from_disk().unwrap_or_default()
    }

    fn load_from_disk() -> Option<Self> {
        let config_path = config_file_path()?;
        let raw = fs::read_to_string(config_path).ok()?;
        toml::from_str(&raw).ok()
    }
}

pub fn config_file_path() -> Option<PathBuf> {
    let project_dirs = ProjectDirs::from("com", "tgomareli", "clicky-desktop")?;
    Some(project_dirs.config_dir().join("config.toml"))
}

pub fn config_file_modified_at() -> Option<SystemTime> {
    let path = config_file_path()?;
    fs::metadata(path).ok()?.modified().ok()
}
