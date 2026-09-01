//! Configuration: which volumes to index, what to prune, and where things live.
//!
//! Exclusions are applied after the MFT is read, not during it. Reading the
//! MFT is one sequential pass over the whole volume regardless of what we
//! intend to keep, so skipping `C:\Windows` would save nothing at scan time.
//! Pruning afterwards means changing these settings costs no rescan.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where the config file itself lives.
///
/// Fixed, unlike everything it points at: a config file cannot record its own
/// location. `data_dir` and `install_dir` are free to move; this is not.
pub const CONFIG_DIR_ENV: &str = "ProgramData";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Drive letters to index.
    pub drives: Vec<char>,
    /// Absolute paths whose subtrees are pruned from results and size totals.
    pub exclude: Vec<String>,
    /// Include NTFS metadata files (`$MFT`, `$LogFile`, ...) in results.
    pub include_metadata: bool,
    /// Directory holding the index snapshot.
    pub data_dir: PathBuf,
    /// Directory holding the installed `batuta.exe`, if Quick Setup ran.
    pub install_dir: PathBuf,
    /// Global hotkey that opens the search UI, e.g. `Ctrl+Space`.
    pub hotkey: Option<String>,
    /// Keep the whole index resident, rather than letting Windows page it
    /// out when the daemon has been idle.
    ///
    /// The index is always held in memory while the daemon runs; this decides
    /// whether it stays in the working set. Releasing it drops the daemon to a
    /// few megabytes resident and costs a slower first query afterwards, while
    /// the pages fault back in.
    pub keep_in_ram: bool,
    /// SID of the account that ran setup.
    ///
    /// The daemon runs as LocalSystem, so it cannot work out who the human
    /// user is at runtime — asking its own token would name SYSTEM. Without
    /// this recorded at setup time, the pipe would grant access only to
    /// SYSTEM and Administrators, locking out the unelevated CLI the daemon
    /// exists to serve.
    pub owner_sid: Option<String>,
    /// Restrict `data_dir` to the installing user, Administrators and SYSTEM.
    ///
    /// The index describes every file belonging to every user, and
    /// `%ProgramData%` is readable by all local accounts by default.
    pub restrict_index: bool,
}

/// `%ProgramData%\Batuta`, or the current directory if that is unavailable.
pub fn default_root() -> PathBuf {
    std::env::var(CONFIG_DIR_ENV)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("Batuta")
}

impl Default for Config {
    fn default() -> Self {
        let root = default_root();
        Config {
            drives: vec!['C', 'D'],
            exclude: vec![
                r"C:\Windows".into(),
                r"C:\Program Files".into(),
                r"C:\Program Files (x86)".into(),
            ],
            include_metadata: false,
            data_dir: root.clone(),
            install_dir: root.join("bin"),
            hotkey: None,
            keep_in_ram: true,
            owner_sid: None,
            restrict_index: false,
        }
    }
}

impl Config {
    /// `%ProgramData%\Batuta\config.toml`.
    pub fn default_path() -> PathBuf {
        default_root().join("config.toml")
    }

    /// The index snapshot, inside whichever data directory is configured.
    pub fn snapshot_path(&self) -> PathBuf {
        self.data_dir.join("index.bin")
    }

    /// The installed executable, used by the service, task and hotkey helper.
    pub fn exe_path(&self) -> PathBuf {
        self.install_dir.join("batuta.exe")
    }

    /// The default paths Quick Setup offers to exclude.
    pub fn default_exclusions() -> Vec<String> {
        Config::default().exclude
    }

    /// Load from `path`, or return defaults if it does not exist.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(toml::from_str(&text)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e.into()),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, toml::to_string_pretty(self)?)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_requested_scope() {
        let c = Config::default();
        assert_eq!(c.drives, vec!['C', 'D']);
        assert!(c
            .exclude
            .iter()
            .any(|e| e.eq_ignore_ascii_case(r"C:\Windows")));
        assert!(c.exclude.iter().any(|e| e.contains("Program Files (x86)")));
        assert!(!c.include_metadata);
        assert!(c.hotkey.is_none());
    }

    #[test]
    fn round_trips_through_toml() {
        let mut c = Config {
            drives: vec!['C', 'D', 'E'],
            ..Default::default()
        };
        c.exclude.push(r"D:\Temp".into());
        c.hotkey = Some("Ctrl+Space".into());
        c.owner_sid = Some("S-1-5-21-1-2-3-1001".into());
        c.keep_in_ram = false;
        c.restrict_index = true;
        c.data_dir = PathBuf::from(r"E:\BatutaData");
        let text = toml::to_string_pretty(&c).unwrap();
        let back: Config = toml::from_str(&text).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn partial_config_fills_in_defaults() {
        // A config written before the new fields existed must keep loading,
        // or an upgrade would break every existing install.
        let c: Config = toml::from_str("drives = ['E']").unwrap();
        assert_eq!(c.drives, vec!['E']);
        assert!(!c.exclude.is_empty());
        assert_eq!(c.data_dir, Config::default().data_dir);
        assert!(c.hotkey.is_none());
        assert!(c.owner_sid.is_none());
        assert!(c.keep_in_ram, "staying resident is the default");
        assert!(!c.restrict_index);
    }

    #[test]
    fn missing_file_yields_defaults() {
        let c = Config::load(Path::new("does-not-exist-anywhere.toml")).unwrap();
        assert_eq!(c.drives, Config::default().drives);
    }

    #[test]
    fn paths_derive_from_the_configured_directories() {
        let c = Config {
            data_dir: PathBuf::from(r"E:\Data"),
            install_dir: PathBuf::from(r"E:\Apps\Batuta"),
            ..Default::default()
        };
        assert_eq!(c.snapshot_path(), PathBuf::from(r"E:\Data\index.bin"));
        assert_eq!(c.exe_path(), PathBuf::from(r"E:\Apps\Batuta\batuta.exe"));
    }

    #[test]
    fn a_relocated_data_dir_moves_the_index_with_it() {
        // The whole point of step 4: the snapshot must follow the setting
        // rather than staying at the compiled-in default.
        let a = Config::default();
        let b = Config {
            data_dir: PathBuf::from(r"D:\Elsewhere"),
            ..Default::default()
        };
        assert_ne!(a.snapshot_path(), b.snapshot_path());
        assert!(b.snapshot_path().starts_with(r"D:\Elsewhere"));
    }
}
