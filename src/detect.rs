//! Local detection: is a given upstream project present on this machine, where
//! does it live, and which ports is it listening on?
//!
//! Everything here is Linux-only and read-only: `/proc` for processes and
//! sockets, plain filesystem walks for install paths, `systemctl is-active` for
//! unit state. No privileged calls.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::SystemTime;

use serde::Serialize;

use crate::registry::Project;

const CLK_TCK: f64 = 100.0; // Linux default on x86_64/aarch64
const SCAN_DEPTH: usize = 3;
const SCAN_BUDGET: usize = 20_000;
const MAX_INSTALL_HITS: usize = 24;

// ---------------------------------------------------------------- report model

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Running,
    Installed,
    Absent,
}

#[derive(Serialize, Clone, Debug)]
pub struct Installation {
    /// binary | appimage | checkout | config | data
    pub kind: String,
    pub path: String,
    pub detail: Option<String>,
    pub modified_ms: Option<u64>,
}

#[derive(Serialize, Clone, Debug)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub exe: Option<String>,
    pub cmdline: String,
    pub cwd: Option<String>,
    pub started_ms: Option<u64>,
    /// comm | exe | cmdline | cwd — how this process got attributed.
    pub matched_by: String,
    /// direct (is the program) | related (mentions it, e.g. a child using its config)
    pub relation: String,
}

#[derive(Serialize, Clone, Debug)]
pub struct PortBinding {
    pub port: u16,
    pub proto: String,
    pub bind: String,
    /// socket (observed kernel listener) | cmdline (declared on the command line)
    pub sources: Vec<String>,
    pub pid: Option<u32>,
    pub process: Option<String>,
    pub relation: String,
    pub url: Option<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Service {
    pub unit: String,
    pub state: String,
    pub active: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct ProjectReport {
    pub slug: String,
    pub display_name: String,
    pub repo: String,
    pub repo_url: String,
    pub status: Status,
    pub installations: Vec<Installation>,
    pub processes: Vec<ProcessInfo>,
    /// Ports held by the project's own processes.
    pub ports: Vec<PortBinding>,
    /// Ports held by processes that merely reference it (child runtimes, sidecars).
    pub related_ports: Vec<PortBinding>,
    pub services: Vec<Service>,
    pub notes: Vec<String>,
}

#[derive(Serialize, Clone, Debug)]
pub struct Timeline {
    pub procs_ms: u64,
    pub sockets_ms: u64,
    pub paths_ms: u64,
    pub services_ms: u64,
}

#[derive(Serialize, Clone, Debug)]
pub struct Snapshot {
    pub generated_ms: u64,
    pub hostname: String,
    pub took_ms: u64,
    pub timeline: Timeline,
    pub scan_roots: Vec<String>,
    pub projects: Vec<ProjectReport>,
}

// ------------------------------------------------------------------- /proc io

struct ProcEntry {
    pid: u32,
    comm: String,
    exe: Option<PathBuf>,
    cwd: Option<PathBuf>,
    cmdline: Vec<String>,
    started_ms: Option<u64>,
}

struct Listener {
    inode: u64,
    port: u16,
    addr: String,
    proto: &'static str,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn file_mtime_ms(meta: &fs::Metadata) -> Option<u64> {
    let secs = meta.mtime();
    let nanos = meta.mtime_nsec();
    u64::try_from(secs).ok().map(|s| s * 1000 + (nanos as u64) / 1_000_000)
}

fn uptime_secs() -> Option<f64> {
    fs::read_to_string("/proc/uptime")
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Field 22 of `/proc/<pid>/stat`: process start time in clock ticks since boot.
fn process_start_ticks(pid: u32) -> Option<f64> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let tail = stat.rsplit_once(')')?.1;
    tail.split_whitespace().nth(19)?.parse().ok()
}

fn read_procs() -> Vec<ProcEntry> {
    let mut out = Vec::new();
    let Ok(dir) = fs::read_dir("/proc") else {
        return out;
    };
    let uptime = uptime_secs();
    let now_s = now_ms() as f64 / 1000.0;

    for entry in dir.flatten() {
        let fname = entry.file_name();
        let Some(pid) = fname.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let base = Path::new("/proc").join(&fname);
        let raw = fs::read(base.join("cmdline")).unwrap_or_default();
        let cmdline: Vec<String> = raw
            .split(|b| *b == 0)
            .filter(|s| !s.is_empty())
            .map(|s| String::from_utf8_lossy(s).into_owned())
            .collect();
        if cmdline.is_empty() {
            continue; // kernel thread
        }
        let comm = fs::read_to_string(base.join("comm"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let exe = fs::read_link(base.join("exe")).ok();
        let cwd = fs::read_link(base.join("cwd")).ok();

        let started_ms = match (uptime, process_start_ticks(pid)) {
            (Some(up), Some(ticks)) => {
                let age = up - ticks / CLK_TCK;
                Some((now_s - age.max(0.0)) as u64 * 1000)
            }
            _ => None,
        };

        out.push(ProcEntry {
            pid,
            comm,
            exe,
            cwd,
            cmdline,
            started_ms,
        });
    }
    out
}

/// The kernel prints `s_addr` as a `%08X` u32, i.e. the network-order bytes read
/// in host order. `to_ne_bytes` undoes exactly that, on any endianness.
fn decode_v4(hex: &str) -> Option<String> {
    let word = u32::from_str_radix(hex, 16).ok()?;
    let b = word.to_ne_bytes();
    Some(format!("{}.{}.{}.{}", b[0], b[1], b[2], b[3]))
}

fn decode_v6(hex: &str) -> Option<String> {
    if hex.len() != 32 {
        return None;
    }
    let mut bytes = [0u8; 16];
    for (i, chunk) in hex.as_bytes().chunks(8).enumerate() {
        let word = u32::from_str_radix(std::str::from_utf8(chunk).ok()?, 16).ok()?;
        // Same host-order-per-word trick as the v4 case.
        bytes[i * 4..i * 4 + 4].copy_from_slice(&word.to_ne_bytes());
    }
    let addr = std::net::Ipv6Addr::from(bytes);
    if addr.is_unspecified() {
        return Some("::".into());
    }
    if let Some(v4) = addr.to_ipv4_mapped() {
        return Some(format!("{v4}"));
    }
    Some(addr.to_string())
}

fn read_listeners() -> Vec<Listener> {
    let mut out = Vec::new();
    for (path, proto, v6) in [
        ("/proc/net/tcp", "tcp", false),
        ("/proc/net/tcp6", "tcp6", true),
    ] {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 10 || f[3] != "0A" {
                continue; // 0A = TCP_LISTEN
            }
            let Some((addr_hex, port_hex)) = f[1].split_once(':') else {
                continue;
            };
            let (Ok(port), Ok(inode)) = (
                u16::from_str_radix(port_hex, 16),
                f[9].parse::<u64>(),
            ) else {
                continue;
            };
            let addr = if v6 {
                decode_v6(addr_hex)
            } else {
                decode_v4(addr_hex)
            };
            let Some(addr) = addr else { continue };
            out.push(Listener {
                inode,
                port,
                addr,
                proto,
            });
        }
    }
    out
}

/// Map listening-socket inode -> owning pid, but only for the pids we already
/// care about. Walking every fd of every process costs ~1s on a busy box; this
/// costs microseconds.
fn socket_owners(wanted: &HashSet<u64>, pids: &HashSet<u32>) -> HashMap<u64, u32> {
    let mut owners = HashMap::new();
    if wanted.is_empty() || pids.is_empty() {
        return owners;
    }
    for pid in pids {
        let Ok(fds) = fs::read_dir(format!("/proc/{pid}/fd")) else {
            continue;
        };
        for fd in fds.flatten() {
            let Ok(target) = fs::read_link(fd.path()) else {
                continue;
            };
            let target = target.to_string_lossy();
            let Some(rest) = target.strip_prefix("socket:[") else {
                continue;
            };
            let Some(num) = rest.strip_suffix(']') else {
                continue;
            };
            if let Ok(inode) = num.parse::<u64>() {
                if wanted.contains(&inode) {
                    owners.insert(inode, *pid);
                }
            }
        }
    }
    owners
}

// ------------------------------------------------------------------ matching

/// Exact / prefix match, so `zen-gate` catches `zen-gate-panel` and `zg.AppImage`
/// but `zg` never catches `zgrep`.
fn name_matches(name: &str, project: &Project) -> bool {
    let name = name.to_ascii_lowercase();
    project.aliases.iter().any(|alias| {
        name == *alias
            || name
                .strip_prefix(alias.as_str())
                .is_some_and(|rest| rest.starts_with(['-', '_', '.']))
    })
}

fn text_mentions(text: &str, project: &Project) -> bool {
    let lower = text.to_ascii_lowercase();
    project
        .substring_hints()
        .iter()
        .any(|hint| lower.contains(hint))
}

/// Only arguments that are shaped like a filesystem path count as a tie to a
/// project. Otherwise `grep zen-gate` or `curl -d '{"url":"owner/zen-gate"}'`
/// would invent processes that have nothing to do with it.
fn path_like(token: &str) -> bool {
    let value = token.split_once('=').map(|(_, v)| v).unwrap_or(token);
    value.starts_with('/')
        || value.starts_with("./")
        || value.starts_with("../")
        || value.starts_with('~')
}

fn cmdline_mentions(project: &Project, args: &[String]) -> bool {
    args.iter()
        .any(|arg| path_like(arg) && text_mentions(arg, project))
}

/// Returns how a process is tied to the project, and whether that tie is direct
/// (`zen-gate` itself) or incidental (`sing-box` reading `~/.config/zen-gate`).
fn proc_match(project: &Project, p: &ProcEntry) -> Option<(&'static str, &'static str)> {
    if name_matches(&p.comm, project) {
        return Some(("comm", "direct"));
    }
    if let Some(exe) = p.exe.as_deref().and_then(|e| e.file_name()) {
        if name_matches(&exe.to_string_lossy(), project) {
            return Some(("exe", "direct"));
        }
    }
    if cmdline_mentions(project, &p.cmdline) {
        return Some(("cmdline", "related"));
    }
    if p.cwd.as_deref().is_some_and(|cwd| text_mentions(&cwd.to_string_lossy(), project)) {
        return Some(("cwd", "related"));
    }
    None
}

/// `-port 18787`, `--port=18787`, `-listen 127.0.0.1:18787`.
fn declared_ports(cmdline: &[String]) -> Vec<u16> {
    const KEYS: [&str; 6] = ["port", "p", "listen", "bind", "addr", "address"];
    let mut ports = Vec::new();
    for (i, arg) in cmdline.iter().enumerate() {
        if !arg.starts_with('-') {
            continue;
        }
        let stripped = arg.trim_start_matches('-');
        let (key, inline) = match stripped.split_once('=') {
            Some((k, v)) => (k, Some(v.to_string())),
            None => (stripped, None),
        };
        if !KEYS.contains(&key.to_ascii_lowercase().as_str()) {
            continue;
        }
        let value = inline.or_else(|| cmdline.get(i + 1).cloned());
        if let Some(port) = value.as_deref().and_then(parse_port) {
            ports.push(port);
        }
    }
    ports.sort_unstable();
    ports.dedup();
    ports
}

fn parse_port(value: &str) -> Option<u16> {
    let candidate = if value.chars().all(|c| c.is_ascii_digit()) {
        value
    } else {
        value.rsplit(':').next().unwrap_or(value)
    };
    if candidate.is_empty() || !candidate.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    candidate.parse().ok()
}

// ------------------------------------------------------------- path discovery

pub fn default_roots() -> Vec<PathBuf> {
    let mut roots = vec![
        PathBuf::from("/opt"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/local/lib"),
        PathBuf::from("/srv"),
    ];
    for home in homes() {
        roots.push(home.join(".config"));
        roots.push(home);
    }
    roots
}

const SKIP_DIRS: [&str; 18] = [
    "node_modules",
    "target",
    "vendor",
    ".git",
    ".cache",
    ".venv",
    "venv",
    "__pycache__",
    ".npm",
    ".bun",
    ".cargo",
    ".rustup",
    ".hermes",
    ".pi",
    "site-packages",
    "dist",
    "build",
    "go",
];

fn git_remote(path: &Path) -> Option<String> {
    let config = fs::read_to_string(path.join(".git/config")).ok()?;
    config.lines().find_map(|line| {
        let line = line.trim();
        line.strip_prefix("url = ")
            .or_else(|| line.strip_prefix("url="))
            .map(|url| url.trim().to_string())
    })
}

/// One filesystem walk serves every project: the roots are the expensive part,
/// not the name comparison.
struct Finder<'a> {
    projects: &'a [Project],
    hits: Vec<Vec<Installation>>,
    seen: Vec<HashSet<String>>,
    visited: usize,
}

impl<'a> Finder<'a> {
    fn new(projects: &'a [Project]) -> Self {
        Self {
            projects,
            hits: vec![Vec::new(); projects.len()],
            seen: vec![HashSet::new(); projects.len()],
            visited: 0,
        }
    }

    fn record(
        &mut self,
        idx: usize,
        kind: &str,
        path: &Path,
        meta: Option<&fs::Metadata>,
        detail: Option<String>,
    ) {
        if self.hits[idx].len() >= MAX_INSTALL_HITS {
            return;
        }
        let key = path.to_string_lossy().into_owned();
        if !self.seen[idx].insert(key.clone()) {
            return;
        }
        self.hits[idx].push(Installation {
            kind: kind.to_string(),
            path: key,
            detail,
            modified_ms: meta.and_then(file_mtime_ms),
        });
    }

    /// Walk a root; `depth` counts hops from it. Symlinks are not followed so a
    /// loop cannot eat the whole budget.
    fn walk(&mut self, dir: &Path, depth: usize) {
        if depth > SCAN_DEPTH || self.visited > SCAN_BUDGET {
            return;
        }
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            self.visited += 1;
            let path = entry.path();
            let Ok(meta) = entry.metadata() else { continue }; // ignores symlinks
            let name = entry.file_name().to_string_lossy().into_owned();

            for idx in 0..self.projects.len() {
                if !name_matches(&name, &self.projects[idx]) {
                    continue;
                }
                if meta.is_dir() {
                    let detail = git_remote(&path);
                    let kind = if detail.is_some() { "checkout" } else { "dir" };
                    self.record(idx, kind, &path, Some(&meta), detail);
                } else {
                    let stem = Path::new(&name)
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_else(|| name.clone());
                    let kind = if name.to_ascii_lowercase().ends_with(".appimage") {
                        "appimage"
                    } else if meta.mode() & 0o111 != 0 && name_matches(&stem, &self.projects[idx]) {
                        "binary"
                    } else {
                        "file"
                    };
                    let detail = size_label(meta.len());
                    self.record(idx, kind, &path, Some(&meta), Some(detail));
                }
            }

            if meta.is_dir() && !SKIP_DIRS.contains(&name.as_str()) {
                self.walk(&path, depth + 1);
            }
        }
    }
}

fn size_label(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= MB {
        format!("{:.1} MB", bytes as f64 / MB)
    } else {
        format!("{:.0} KB", bytes as f64 / 1024.0)
    }
}

fn scan_paths(projects: &[Project], roots: &[PathBuf]) -> Vec<Vec<Installation>> {
    let mut finder = Finder::new(projects);
    // Config dirs first: a `~/.config/<slug>` that also shows up in a walk keeps
    // its more specific kind (dedupe keeps the first record).
    let homes = homes();
    for (idx, project) in projects.iter().enumerate() {
        for rel in &project.config_dirs {
            for home in &homes {
                let path = home.join(rel);
                if let Ok(meta) = fs::metadata(&path) {
                    finder.record(idx, "config", &path, Some(&meta), None);
                }
            }
        }
    }
    for root in roots {
        finder.walk(root, 0);
    }
    finder.hits
}

/// Every home directory worth looking in.
///
/// `$HOME` is not enough: under systemd the unit has no HOME, and a panel that
/// silently stops finding `~/.config/<slug>` because of that is worse than
/// useless. So: env, then the passwd entry for our own uid, then `/root` and
/// `/home/*` as a backstop.
fn homes() -> Vec<PathBuf> {
    let mut homes: Vec<PathBuf> = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        homes.push(home);
    }
    if let Some(home) = passwd_home() {
        homes.push(home);
    }
    let root = PathBuf::from("/root");
    if root.is_dir() {
        homes.push(root);
    }
    if let Ok(users) = fs::read_dir("/home") {
        for user in users.flatten() {
            if user.path().is_dir() {
                homes.push(user.path());
            }
        }
    }
    homes.dedup();
    homes
}

/// Home of the uid this process runs as, straight out of `/etc/passwd`.
fn passwd_home() -> Option<PathBuf> {
    let uid = fs::read_to_string("/proc/self/status")
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("Uid:"))?
        .split_whitespace()
        .next()?
        .to_string();
    let passwd = fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        let mut f = line.split(':');
        let (_, _, user_uid) = (f.next()?, f.next()?, f.next()?);
        if user_uid != uid {
            return None;
        }
        let home = f.nth(2)?; // fields: passwd, gid, gecos, dir
        Some(PathBuf::from(home))
    })
}

// ------------------------------------------------------------------- systemd

fn scan_services(project: &Project) -> Vec<Service> {
    const UNIT_DIRS: [&str; 4] = [
        "/etc/systemd/system",
        "/usr/lib/systemd/system",
        "/lib/systemd/system",
        "/run/systemd/system",
    ];
    let mut units: Vec<String> = Vec::new();
    for dir in UNIT_DIRS {
        let Ok(entries) = fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".service") || units.contains(&name) {
                continue;
            }
            let stem = name.trim_end_matches(".service");
            let by_name = name_matches(stem, project);
            let by_content = fs::read_to_string(entry.path())
                .map(|body| text_mentions(&body, project))
                .unwrap_or(false);
            if by_name || by_content {
                units.push(name);
            }
        }
    }
    units.sort();
    units
        .into_iter()
        .map(|unit| {
            let state = Command::new("systemctl")
                .args(["is-active", &unit])
                .output()
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "unknown".to_string());
            Service {
                active: state == "active",
                unit,
                state,
            }
        })
        .collect()
}

// ------------------------------------------------------------------ assembly

pub fn detect(projects: &[Project], roots: &[PathBuf]) -> Snapshot {
    let started = std::time::Instant::now();
    let procs = read_procs();
    let procs_ms = started.elapsed().as_millis() as u64;

    // Match first, then attribute sockets only for the pids we already matched.
    let matches: Vec<Vec<Match<'_>>> = projects
        .iter()
        .map(|project| {
            procs
                .iter()
                .filter_map(|p| proc_match(project, p).map(|(by, rel)| Match { proc: p, by, rel }))
                .collect()
        })
        .collect();

    let mut candidate_pids: HashSet<u32> = HashSet::new();
    for m in &matches {
        for hit in m {
            candidate_pids.insert(hit.proc.pid);
        }
    }

    let listeners = read_listeners();
    let wanted: HashSet<u64> = listeners.iter().map(|l| l.inode).collect();
    let owners = socket_owners(&wanted, &candidate_pids);
    let sockets_ms = started.elapsed().as_millis() as u64 - procs_ms;

    let installations = scan_paths(projects, roots);
    let paths_ms = started.elapsed().as_millis() as u64 - procs_ms - sockets_ms;

    let services_started = std::time::Instant::now();
    let reports = projects
        .iter()
        .zip(matches)
        .zip(installations)
        .map(|((project, matched), found)| report_one(project, &matched, &listeners, &owners, found))
        .collect();

    Snapshot {
        generated_ms: now_ms(),
        hostname: fs::read_to_string("/proc/sys/kernel/hostname")
            .map(|h| h.trim().to_string())
            .unwrap_or_else(|_| "unknown".into()),
        took_ms: started.elapsed().as_millis() as u64,
        timeline: Timeline {
            procs_ms,
            sockets_ms,
            paths_ms,
            services_ms: services_started.elapsed().as_millis() as u64,
        },
        scan_roots: roots.iter().map(|r| r.to_string_lossy().into_owned()).collect(),
        projects: reports,
    }
}

struct Match<'a> {
    proc: &'a ProcEntry,
    by: &'static str,
    rel: &'static str,
}

fn report_one(
    project: &Project,
    matched: &[Match<'_>],
    listeners: &[Listener],
    owners: &HashMap<u64, u32>,
    installations: Vec<Installation>,
) -> ProjectReport {
    let by_pid: HashMap<u32, (&str, &'static str)> = matched
        .iter()
        .map(|m| (m.proc.pid, (m.proc.comm.as_str(), m.rel)))
        .collect();

    // Ports observed in the kernel, owned by a matched process.
    let mut ports: Vec<PortBinding> = Vec::new();
    for listener in listeners {
        let Some((pid, (comm, rel))) = owners.get(&listener.inode).and_then(|pid| {
            by_pid.get(pid).map(|info| (*pid, info))
        }) else {
            continue;
        };
        ports.push(PortBinding {
            port: listener.port,
            proto: listener.proto.to_string(),
            bind: listener.addr.clone(),
            sources: vec!["socket".into()],
            pid: Some(pid),
            process: Some((*comm).to_string()),
            relation: (*rel).to_string(),
            url: Some(format!(
                "http://{}:{}/",
                host_for_url(&listener.addr),
                listener.port
            )),
        });
    }

    // Ports declared on the command line: covers a process that binds before we
    // catch it, and keeps us honest when the socket belongs to a child.
    for m in matched {
        for port in declared_ports(&m.proc.cmdline) {
            match ports
                .iter_mut()
                .find(|b| b.port == port && b.proto.starts_with("tcp"))
            {
                Some(existing) => {
                    if !existing.sources.iter().any(|s| s == "cmdline") {
                        existing.sources.push("cmdline".into());
                    }
                }
                None => ports.push(PortBinding {
                    port,
                    proto: "tcp".into(),
                    bind: "declared".into(),
                    sources: vec!["cmdline".into()],
                    pid: Some(m.proc.pid),
                    process: Some(m.proc.comm.clone()),
                    relation: m.rel.to_string(),
                    url: Some(format!("http://127.0.0.1:{port}/")),
                }),
            }
        }
    }
    let related_ports: Vec<PortBinding> = ports
        .iter()
        .filter(|b| b.relation == "related")
        .cloned()
        .collect();
    ports.retain(|b| b.relation == "direct");
    ports.sort_by_key(|b| (b.port, b.proto.clone()));

    let services = scan_services(project);
    let direct: Vec<&Match<'_>> = matched.iter().filter(|m| m.rel == "direct").collect();
    let related: Vec<&Match<'_>> = matched.iter().filter(|m| m.rel == "related").collect();

    let mut notes = Vec::new();
    if !matched.is_empty() && listeners.is_empty() {
        notes.push("进程在跑，但本机没有任何 TCP 监听。".to_string());
    }
    if !matched.is_empty() && !ports.iter().any(|b| b.sources.iter().any(|s| s == "socket")) && !ports.is_empty() {
        notes.push("端口是从命令行参数推出来的，没在 /proc/net/* 里对上监听 socket。".to_string());
    }
    if let Some(b) = ports.iter().find(|b| b.bind == "0.0.0.0" || b.bind == "::") {
        notes.push(format!("端口 {} 绑定在全接口，不只听 loopback。", b.port));
    }
    if installations.is_empty() && !matched.is_empty() {
        notes.push("没在扫描根里找到安装文件（可能在容器里跑，或装在扫描根之外）。".to_string());
    }
    if direct.is_empty() && !related.is_empty() {
        notes.push("只找到引用它的进程，没找到本体进程。".to_string());
    }
    if !related.is_empty() {
        let names: Vec<&str> = related.iter().map(|m| m.proc.comm.as_str()).collect();
        notes.push(format!(
            "{} 也引用了它的配置或路径，这些进程的 {} 个端口折叠在「相关进程」里。",
            names.join(" / "),
            related_ports.len()
        ));
    }
    if installations.iter().any(|i| i.kind == "checkout") {
        let remotes: Vec<&str> = installations
            .iter()
            .filter(|i| i.kind == "checkout")
            .filter_map(|i| i.detail.as_deref())
            .collect();
        if !remotes.is_empty() && !remotes.iter().any(|r| text_mentions(r, project)) {
            notes.push("找到的 git 检出 remote 跟目标仓库对不上，可能只是同名目录。".to_string());
        }
    }

    let status = if !matched.is_empty() {
        Status::Running
    } else if !installations.is_empty() {
        Status::Installed
    } else {
        Status::Absent
    };

    ProjectReport {
        slug: project.slug.to_string(),
        display_name: project.display_name.to_string(),
        repo: project.repo.to_string(),
        repo_url: project.repo_url(),
        status,
        processes: matched
            .iter()
            .map(|m| ProcessInfo {
                pid: m.proc.pid,
                name: m.proc.comm.clone(),
                exe: m.proc.exe.as_ref().map(|e| e.to_string_lossy().into_owned()),
                cmdline: m.proc.cmdline.join(" "),
                cwd: m.proc.cwd.as_ref().map(|c| c.to_string_lossy().into_owned()),
                started_ms: m.proc.started_ms,
                matched_by: m.by.to_string(),
                relation: m.rel.to_string(),
            })
            .collect(),
        installations,
        ports,
        related_ports,
        services,
        notes,
    }
}

fn host_for_url(bind: &str) -> String {
    match bind {
        "0.0.0.0" | "::" => "127.0.0.1".to_string(),
        other => other.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zen_gate() -> Project {
        Project::from_url("https://github.com/LAGcomcom/zen-gate", "zen_gate, zengate, zg").unwrap()
    }

    #[test]
    fn name_match_accepts_prefix_but_not_short_substring() {
        let p = zen_gate();
        assert!(name_matches("zen-gate", &p));
        assert!(name_matches("zen-gate-panel", &p));
        assert!(name_matches("zg.AppImage", &p));
        // "zg" must not swallow zgrep
        assert!(!name_matches("zgrep", &p));
        assert!(!name_matches("new-api", &p));
    }

    #[test]
    fn short_aliases_are_not_used_for_substring_matching() {
        let p = zen_gate();
        assert!(text_mentions("/root/.config/zen-gate/subs", &p));
        assert!(!text_mentions("/usr/bin/zgrep -r", &p));
    }

    #[test]
    fn cmdline_ties_need_a_path_shaped_argument() {
        let p = zen_gate();
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // sidecars that actually point at its files
        assert!(cmdline_mentions(
            &p,
            &args(&["/usr/local/bin/sing-box", "-c", "/root/.config/zen-gate/subs/x.json"])
        ));
        assert!(cmdline_mentions(
            &p,
            &args(&["/usr/bin/x", "--config=/root/.config/zen-gate/app.toml"])
        ));
        assert!(cmdline_mentions(
            &p,
            &args(&["/opt/zen-gate-panel/panel-proxy", "-listen", "127.0.0.1:18788"])
        ));
        // things that merely mention the name
        assert!(!cmdline_mentions(&p, &args(&["/usr/bin/grep", "zen-gate"])));
        assert!(!cmdline_mentions(
            &p,
            &args(&["/usr/bin/curl", "-d", "{\"url\":\"apernet/zen-gate\"}"])
        ));
    }

    #[test]
    fn ports_are_read_from_several_flag_styles() {
        let args = |s: &str| s.split_whitespace().map(String::from).collect::<Vec<_>>();
        assert_eq!(declared_ports(&args("bin -port 18787")), vec![18787]);
        assert_eq!(declared_ports(&args("bin --port=18788")), vec![18788]);
        assert_eq!(declared_ports(&args("bin -listen 100.64.0.1:18789")), vec![18789]);
        assert_eq!(declared_ports(&args("bin --listen=[::1]:18790")), vec![18790]);
        assert!(declared_ports(&args("bin run -c /root/.config/zen-gate/x.json")).is_empty());
    }

    #[test]
    fn proc_addr_decoding() {
        assert_eq!(decode_v4("0100007F").as_deref(), Some("127.0.0.1"));
        assert_eq!(decode_v4("00000000").as_deref(), Some("0.0.0.0"));
        // ::1 as printed by the kernel in host byte order
        assert_eq!(
            decode_v6("00000000000000000000000001000000").as_deref(),
            Some("::1")
        );
        // v4-mapped (what Docker and dual-stack binds show) collapses to plain v4
        assert_eq!(
            decode_v6("0000000000000000FFFF0000B80D0114").as_deref(),
            Some("20.1.13.184")
        );
        assert_eq!(decode_v6("00000000000000000000000000000000").as_deref(), Some("::"));
    }
}
