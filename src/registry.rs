//! The set of upstream projects this panel watches, plus how they are stored.
//!
//! Entries are runtime data now, not compile-time constants: the panel adds them
//! from the UI (`POST /api/projects`), persists them to `projects.json`, and the
//! detection code stays generic.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub const DEFAULT_STATE: &str = "/var/lib/proxy-distributor/projects.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Project {
    /// Stable id, lowercase, also the folder hint (`~/.config/<slug>`).
    pub slug: String,
    pub display_name: String,
    /// GitHub `owner/name`.
    pub repo: String,
    /// Names that identify the project in paths and process names.
    pub aliases: Vec<String>,
    /// Directories under a home dir that belong to the project.
    pub config_dirs: Vec<String>,
}

impl Project {
    pub fn repo_url(&self) -> String {
        format!("https://github.com/{}", self.repo)
    }

    /// Aliases long enough to be matched as a substring without drowning in
    /// false positives (`zg` would hit `zgrep`, `zen-gate` would not).
    pub fn substring_hints(&self) -> Vec<&str> {
        self.aliases
            .iter()
            .map(String::as_str)
            .filter(|a| a.len() >= 4)
            .collect()
    }

    /// Build an entry out of a pasted GitHub link plus optional extra aliases.
    pub fn from_url(input: &str, extra_aliases: &str) -> Result<Self, String> {
        let repo = parse_github(input)?;
        let name = repo.rsplit('/').next().unwrap_or(&repo).to_string();
        let slug = name.to_ascii_lowercase();

        let mut aliases = vec![slug.clone()];
        for alias in extra_aliases
            .split([',', '，', ' ', '\t', '\n'])
            .map(|a| a.trim().to_ascii_lowercase())
            .filter(|a| !a.is_empty())
        {
            if !aliases.contains(&alias) {
                aliases.push(alias);
            }
        }
        for alias in &aliases {
            if !alias
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
            {
                return Err(format!("别名里有非法字符: {alias}"));
            }
        }

        let mut config_dirs = vec![format!(".config/{slug}")];
        if slug.contains('_') {
            config_dirs.push(format!(".config/{}", slug.replace('_', "-")));
        }

        Ok(Project {
            display_name: name,
            slug,
            repo,
            aliases,
            config_dirs,
        })
    }
}

/// Pull `owner/name` out of whatever was pasted: full URL, ssh form, bare
/// `owner/name`, with `.git`, trailing slash, query or fragment.
pub fn parse_github(input: &str) -> Result<String, String> {
    let raw = input.trim();
    if raw.is_empty() {
        return Err("链接是空的".to_string());
    }
    let mut rest = raw;
    for scheme in ["https://", "http://", "ssh://"] {
        if let Some(stripped) = rest.strip_prefix(scheme) {
            rest = stripped;
            break;
        }
    }
    if let Some(stripped) = rest.strip_prefix("git@") {
        rest = stripped;
    }
    if let Some(stripped) = rest.strip_prefix("www.") {
        rest = stripped;
    }

    // Drop credentials, query and fragment.
    for cut in ['?', '#'] {
        if let Some((head, _)) = rest.split_once(cut) {
            rest = head;
        }
    }

    // `github.com/owner/name` and `github.com:owner/name` (ssh) both put the
    // host first; a bare `owner/name` does not.
    let (head, tail) = match rest.split_once(['/', ':']) {
        Some(parts) => parts,
        None => return Err(format!("看不懂这个链接: {raw}（要 owner/name）")),
    };
    let (owner, name_part) = if head.contains('.') || head.eq_ignore_ascii_case("github.com") {
        if !head.eq_ignore_ascii_case("github.com") {
            return Err(format!("只认 github.com，这个是 {head}"));
        }
        match tail.split_once('/') {
            Some((owner, rest)) => (owner, rest),
            None => return Err("链接里没有仓库名".to_string()),
        }
    } else {
        (head, tail)
    };
    let name = name_part.trim_matches('/').split('/').next().unwrap_or("");
    let name = name.strip_suffix(".git").unwrap_or(name);
    if name.is_empty() || owner.is_empty() {
        return Err("链接里没有仓库名".to_string());
    }

    for part in [owner, name] {
        if !part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        {
            return Err(format!("仓库路径里有非法字符: {part}"));
        }
    }
    Ok(format!("{owner}/{name}"))
}

/// On-disk store. Missing file = first boot, so seed with the projects this box
/// has been tracking since before adding became a thing.
pub struct Store {
    path: PathBuf,
}

impl Store {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn from_env() -> Self {
        let path = std::env::var_os("PROXY_DISTRIBUTOR_STATE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(DEFAULT_STATE));
        Self::new(path)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Vec<Project> {
        match fs::read_to_string(&self.path) {
            Ok(text) => match serde_json::from_str::<Vec<Project>>(&text) {
                Ok(projects) => projects,
                Err(err) => {
                    eprintln!("{} 解析失败（用种子数据顶上）: {err}", self.path.display());
                    seed()
                }
            },
            Err(_) => seed(),
        }
    }

    /// Write-then-rename: a half-written file must never replace a good one.
    pub fn save(&self, projects: &[Project]) -> Result<(), String> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("创建 {} 失败: {e}", dir.display()))?;
        }
        let body =
            serde_json::to_string_pretty(projects).map_err(|e| format!("序列化失败: {e}"))?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, body).map_err(|e| format!("写 {} 失败: {e}", tmp.display()))?;
        fs::rename(&tmp, &self.path).map_err(|e| format!("重命名到 {} 失败: {e}", self.path.display()))
    }
}

pub fn seed() -> Vec<Project> {
    vec![Project {
        slug: "zen-gate".to_string(),
        display_name: "Zen Gate".to_string(),
        repo: "LAGcomcom/zen-gate".to_string(),
        aliases: vec![
            "zen-gate".to_string(),
            "zen_gate".to_string(),
            "zengate".to_string(),
            "zg".to_string(),
        ],
        config_dirs: vec![".config/zen-gate".to_string()],
    }]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_forms_people_actually_paste() {
        for input in [
            "https://github.com/LAGcomcom/zen-gate",
            "https://github.com/LAGcomcom/zen-gate.git",
            "https://github.com/LAGcomcom/zen-gate/",
            "https://github.com/LAGcomcom/zen-gate/tree/main",
            "git@github.com:LAGcomcom/zen-gate.git",
            "github.com/LAGcomcom/zen-gate",
            "LAGcomcom/zen-gate",
            "  https://www.github.com/LAGcomcom/zen-gate?tab=readme  ",
        ] {
            assert_eq!(parse_github(input).unwrap(), "LAGcomcom/zen-gate", "{input}");
        }
    }

    #[test]
    fn rejects_everything_else() {
        for input in [
            "",
            "zen-gate",
            "https://gitlab.com/owner/repo",
            "https://github.com/owner",
            "https://github.com/owner/re po",
        ] {
            assert!(parse_github(input).is_err(), "{input} 应该被拒");
        }
    }

    #[test]
    fn derives_slug_aliases_and_config_dir() {
        let p = Project::from_url("https://github.com/LAGcomcom/Zen-Gate.git", "zg, zen_gate").unwrap();
        assert_eq!(p.slug, "zen-gate");
        assert_eq!(p.repo, "LAGcomcom/Zen-Gate");
        assert_eq!(p.aliases, vec!["zen-gate", "zg", "zen_gate"]);
        assert_eq!(p.config_dirs, vec![".config/zen-gate"]);
        assert_eq!(p.repo_url(), "https://github.com/LAGcomcom/Zen-Gate");
    }

    #[test]
    fn substring_hints_skip_short_aliases() {
        let p = Project::from_url("owner/zen-gate", "zg").unwrap();
        assert_eq!(p.substring_hints(), vec!["zen-gate"]);
    }
}
