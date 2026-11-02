//! The set of upstream projects this panel watches.
//!
//! Add an entry here to make the panel start tracking a new project. Detection
//! itself is generic (see `detect.rs`) — this file is the only per-project code.

pub struct Project {
    /// Stable id, lowercase, also used as a filename/folder hint.
    pub slug: &'static str,
    pub display_name: &'static str,
    /// GitHub `owner/name`.
    pub repo: &'static str,
    /// Short prefixes that identify the project in paths and process names
    /// (`zen-gate`, `zg` → `zg.AppImage`, `zen-gate-panel`).
    pub aliases: &'static [&'static str],
    /// Directories under `$HOME` owned by the project.
    pub config_dirs: &'static [&'static str],
}

impl Project {
    pub fn repo_url(&self) -> String {
        format!("https://github.com/{}", self.repo)
    }

    /// Aliases long enough to be matched as a substring without drowning in
    /// false positives (`zg` would hit `zgrep`, `zen-gate` would not).
    pub fn substring_hints(&self) -> Vec<&'static str> {
        self.aliases
            .iter()
            .copied()
            .filter(|a| a.len() >= 4)
            .collect()
    }
}

pub fn all() -> Vec<Project> {
    vec![
        Project {
            slug: "zen-gate",
            display_name: "Zen Gate",
            repo: "LAGcomcom/zen-gate",
            aliases: &["zen-gate", "zen_gate", "zengate", "zg"],
            config_dirs: &[".config/zen-gate"],
        },
        // Next project goes here.
    ]
}
