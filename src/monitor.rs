//! `asp monitor`: watches RAM and disk, and names the Claude Code sessions
//! behind a drift (a process tree too big, memory pressure, swap, a disk
//! almost full or emptying fast). It measures and names; it never kills or
//! deletes anything.
//!
//! The agent runs it all the time (`spawn`): an alert becomes a macOS
//! notification, a message in the culprit session's Telegram inbox when it
//! listens, and a message to the owner when nobody is to blame or it is
//! critical. `asp monitor watch` prints the same alerts for a script.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const GB: f64 = 1024.0 * 1024.0 * 1024.0;
const GIB: u64 = 1024 * 1024 * 1024;
/// Under memory pressure, the N heaviest sessions are named.
const SESSIONS_UNDER_PRESSURE: usize = 3;
/// An orphan (parent launchd) only counts when it is big.
const ORPHAN_MIN: u64 = GIB;
const SYSTEM_PREFIXES: [&str; 4] = ["/System", "/Applications", "/Library", "/usr"];
/// Seconds between two readings.
const STEP: u64 = 15;
const DISK_HISTORY: f64 = 30.0 * 60.0;
/// One `du -sk` per folder at most every 15 minutes.
const DU_PERIOD: f64 = 15.0 * 60.0;
const DU_KEEP: f64 = 35.0 * 60.0;
const DU_TIMEOUT: Duration = Duration::from_secs(120);
/// No repeat per (session, type) for 20 minutes…
const RENEWAL: f64 = 20.0 * 60.0;
/// …unless the value grew this much.
const MULTIPLIER: f64 = 1.5;

/// `[monitor]` in config.toml, in GB.
#[derive(Deserialize, Clone)]
#[serde(default)]
pub struct Thresholds {
    pub enabled: bool,
    pub session_gb: f64,
    pub session_critical_gb: f64,
    pub swap_gb: f64,
    pub swap_critical_gb: f64,
    pub disk_free_gb: f64,
    pub disk_free_critical_gb: f64,
    /// A free-space drop this big within 15 minutes…
    pub disk_drop_gb: f64,
    /// …is blamed on a session folder that grew this much.
    pub folder_growth_gb: f64,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            enabled: true,
            session_gb: 8.0,
            session_critical_gb: 16.0,
            swap_gb: 8.0,
            swap_critical_gb: 16.0,
            disk_free_gb: 30.0,
            disk_free_critical_gb: 15.0,
            disk_drop_gb: 10.0,
            folder_growth_gb: 1.0,
        }
    }
}

#[derive(Deserialize, Default)]
struct ConfigFile {
    #[serde(default)]
    monitor: Thresholds,
}

pub fn thresholds() -> Thresholds {
    std::fs::read_to_string(crate::telegram::config_file())
        .ok()
        .and_then(|text| toml::from_str::<ConfigFile>(&text).ok())
        .map(|config| config.monitor)
        .unwrap_or_default()
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// "12,3" in French, "12.3" in English.
fn gb(bytes: f64, fr: bool) -> String {
    let text = format!("{:.1}", bytes / GB);
    if fr {
        text.replace('.', ",")
    } else {
        text
    }
}

macro_rules! tr {
    ($fr:expr, $french:literal, $english:literal) => {
        if $fr {
            format!($french)
        } else {
            format!($english)
        }
    };
}

// Reading the system.

#[derive(Clone, Debug, PartialEq)]
pub struct Session {
    pub pid: u32,
    pub name: String,
    pub cwd: String,
}

#[derive(Clone, Debug)]
pub struct Proc {
    pub ppid: u32,
    pub ram: u64,
    pub command: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct TreeProc {
    pub pid: u32,
    pub ram: u64,
    pub name: String,
    pub orphan: bool,
}

#[derive(Clone, Debug)]
pub struct Tree {
    pub session: Session,
    pub ram: u64,
    pub procs: Vec<TreeProc>,
}

pub struct Snapshot {
    pub trees: Vec<Tree>,
    pub top_system: Vec<TreeProc>,
    pub outside: u64,
    pub pressure: i64,
    pub swap: u64,
    pub memtotal: u64,
    pub free: u64,
    pub sessions: Vec<Session>,
    pub home: String,
}

fn short_name(command: &str) -> String {
    let trimmed = command.trim_end_matches('/');
    Path::new(trimmed)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| command.to_string())
}

/// A process's real memory, as Activity Monitor shows it. `ps`'s RSS leaves
/// out compressed memory (2.2 GB of RSS seen for 9 GB real, enough to hide
/// a leak); phys_footprint counts it, and not shared memory.
fn footprint(pid: u32) -> Option<u64> {
    let mut info: libc::rusage_info_v0 = unsafe { std::mem::zeroed() };
    let read = unsafe {
        libc::proc_pid_rusage(
            pid as libc::c_int,
            libc::RUSAGE_INFO_V0,
            &mut info as *mut libc::rusage_info_v0 as *mut libc::rusage_info_t,
        )
    };
    (read == 0).then_some(info.ri_phys_footprint)
}

/// A process's start as Claude Code writes `procStart`: UTC, English, like
/// `ps -o lstart` ("Sun Nov  1 05:08:55 2026"), whitespace collapsed.
fn start_text(seconds: u64) -> String {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let time = seconds as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::gmtime_r(&time, &mut tm) };
    format!(
        "{} {} {} {:02}:{:02}:{:02} {}",
        DAYS[tm.tm_wday.clamp(0, 6) as usize],
        MONTHS[tm.tm_mon.clamp(0, 11) as usize],
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
        tm.tm_year + 1900
    )
}

fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Live sessions of the profiles' `sessions/<pid>.json`. When `starts` is
/// given, `procStart` must match the process start, otherwise the pid was
/// reused after a crash and the session is a ghost. Folders are
/// deduplicated by real path (profiles often share one).
pub fn read_sessions(folders: &[PathBuf], alive: &HashSet<u32>, starts: &HashMap<u32, String>) -> Vec<Session> {
    let mut found: BTreeMap<u32, Session> = BTreeMap::new();
    let mut seen = HashSet::new();
    for folder in folders {
        let Ok(real) = folder.canonicalize() else {
            continue;
        };
        if !seen.insert(real.clone()) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&real) else {
            continue;
        };
        let mut files: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        files.sort();
        for file in files {
            let numbered = file.extension().is_some_and(|e| e == "json")
                && file
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()));
            if !numbered {
                continue;
            }
            let Some(data) = std::fs::read_to_string(&file)
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            else {
                continue;
            };
            let (Some(pid), Some(cwd)) = (data.get("pid").and_then(Value::as_u64), data.get("cwd").and_then(Value::as_str))
            else {
                continue;
            };
            let pid = pid as u32;
            if let (Some(started), Some(written)) = (starts.get(&pid), data.get("procStart").and_then(Value::as_str)) {
                if collapse(written) != *started {
                    continue;
                }
            }
            if alive.contains(&pid) && !found.contains_key(&pid) {
                let name = data
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|n| !n.is_empty())
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("pid-{pid}"));
                found.insert(pid, Session { pid, name, cwd: cwd.to_string() });
            }
        }
    }
    found.into_values().collect()
}

/// Walks up the parents: (nearest ancestor session, root of the chain).
fn root_of(pid: u32, procs: &HashMap<u32, Proc>, sessions: &HashSet<u32>) -> (Option<u32>, u32) {
    let mut seen = HashSet::new();
    let mut current = pid;
    loop {
        if sessions.contains(&current) {
            return (Some(current), current);
        }
        seen.insert(current);
        let parent = procs.get(&current).map_or(0, |p| p.ppid);
        if parent <= 1 || !procs.contains_key(&parent) || seen.contains(&parent) {
            return (None, current);
        }
        current = parent;
    }
}

/// The session whose cwd holds this folder (the deepest wins).
fn session_of_folder<'a>(folder: &str, sessions: &'a [Session]) -> Option<&'a Session> {
    sessions
        .iter()
        .filter(|s| {
            let cwd = s.cwd.trim_end_matches('/');
            folder == cwd || folder.starts_with(&format!("{cwd}/"))
        })
        .max_by_key(|s| s.cwd.trim_end_matches('/').len())
}

/// Each process goes to its nearest ancestor session. A big orphan (parent
/// launchd, outside the system) and its descendants go to the session
/// whose cwd holds theirs, marked orphan. Returns (trees, others).
pub fn attribute(
    procs: &HashMap<u32, Proc>,
    sessions: &[Session],
    cwd_of: impl Fn(u32) -> Option<String>,
) -> (Vec<Tree>, Vec<TreeProc>) {
    let ids: HashSet<u32> = sessions.iter().map(|s| s.pid).collect();
    let mut trees: BTreeMap<u32, Tree> = sessions
        .iter()
        .map(|s| (s.pid, Tree { session: s.clone(), ram: 0, procs: Vec::new() }))
        .collect();
    let mut loose: BTreeMap<u32, Vec<u32>> = BTreeMap::new();
    let mut pids: Vec<u32> = procs.keys().copied().collect();
    pids.sort_unstable();
    for pid in pids {
        match root_of(pid, procs, &ids) {
            (Some(session), _) => add(trees.get_mut(&session), pid, &procs[&pid], false),
            (None, root) => loose.entry(root).or_default().push(pid),
        }
    }
    let mut others = Vec::new();
    for (root, members) in loose {
        let total: u64 = members.iter().map(|p| procs[p].ram).sum();
        let head = &procs[&root];
        let target = (head.ppid == 1
            && total >= ORPHAN_MIN
            && !SYSTEM_PREFIXES.iter().any(|prefix| head.command.starts_with(prefix)))
        .then(|| cwd_of(root))
        .flatten()
        .and_then(|folder| {
            let folder = folder.trim_end_matches('/');
            session_of_folder(if folder.is_empty() { "/" } else { folder }, sessions).map(|s| s.pid)
        });
        for pid in members {
            match target {
                Some(session) => add(trees.get_mut(&session), pid, &procs[&pid], true),
                None => others.push(TreeProc { pid, ram: procs[&pid].ram, name: short_name(&procs[&pid].command), orphan: false }),
            }
        }
    }
    let mut trees: Vec<Tree> = trees.into_values().collect();
    for tree in &mut trees {
        tree.procs.sort_by_key(|p| std::cmp::Reverse(p.ram));
    }
    trees.sort_by_key(|t| std::cmp::Reverse(t.ram));
    others.sort_by_key(|p| std::cmp::Reverse(p.ram));
    (trees, others)
}

fn add(tree: Option<&mut Tree>, pid: u32, proc: &Proc, orphan: bool) {
    if let Some(tree) = tree {
        tree.ram += proc.ram;
        tree.procs.push(TreeProc { pid, ram: proc.ram, name: short_name(&proc.command), orphan });
    }
}

fn sysctl_u64(name: &str) -> Option<u64> {
    let name = std::ffi::CString::new(name).ok()?;
    let mut value: u64 = 0;
    let mut size = std::mem::size_of::<u64>();
    let read = unsafe {
        libc::sysctlbyname(name.as_ptr(), &mut value as *mut u64 as *mut libc::c_void, &mut size, std::ptr::null_mut(), 0)
    };
    (read == 0).then_some(if size == 4 { value & 0xFFFF_FFFF } else { value })
}

fn swap_used() -> u64 {
    let name = c"vm.swapusage";
    let mut usage: libc::xsw_usage = unsafe { std::mem::zeroed() };
    let mut size = std::mem::size_of::<libc::xsw_usage>();
    let read = unsafe {
        libc::sysctlbyname(name.as_ptr(), &mut usage as *mut libc::xsw_usage as *mut libc::c_void, &mut size, std::ptr::null_mut(), 0)
    };
    if read == 0 {
        usage.xsu_used
    } else {
        0
    }
}

fn free_space(path: &str) -> u64 {
    let Ok(path) = std::ffi::CString::new(path) else {
        return 0;
    };
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return 0;
    }
    stat.f_bavail as u64 * stat.f_frsize
}

fn session_folders(home: &Path) -> Vec<PathBuf> {
    crate::claude_config_dirs(home)
        .into_iter()
        .map(|dir| dir.join("sessions"))
        .collect()
}

/// One complete reading of the system.
pub fn read_system() -> Snapshot {
    let home = dirs::home_dir().unwrap_or_default();
    let mut procs = HashMap::new();
    let mut starts = HashMap::new();
    for info in crate::load_process_table() {
        let ram = footprint(info.pid).unwrap_or(0);
        if let Some(bsd) = crate::bsd_info(info.pid) {
            starts.insert(info.pid, start_text(bsd.pbi_start_tvsec));
        }
        procs.insert(info.pid, Proc { ppid: info.ppid, ram, command: info.arg0 });
    }
    let alive: HashSet<u32> = procs.keys().copied().collect();
    let sessions = read_sessions(&session_folders(&home), &alive, &starts);
    let (trees, others) = attribute(&procs, &sessions, crate::get_process_cwd);
    let mut top: Vec<TreeProc> = procs
        .iter()
        .map(|(pid, p)| TreeProc { pid: *pid, ram: p.ram, name: short_name(&p.command), orphan: false })
        .collect();
    top.sort_by_key(|p| std::cmp::Reverse(p.ram));
    top.truncate(3);
    let home_text = home.display().to_string();
    Snapshot {
        trees,
        top_system: top,
        outside: others.iter().map(|p| p.ram).sum(),
        pressure: sysctl_u64("kern.memorystatus_vm_pressure_level").unwrap_or(1) as i64,
        swap: swap_used(),
        memtotal: sysctl_u64("hw.memsize").unwrap_or(0),
        free: free_space(&home_text),
        sessions,
        home: home_text,
    }
}

// Evaluation.

#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Top {
    pub pid: u32,
    pub go: f64,
    pub nom: String,
}

/// The keys are the Python moniteur's, so its readers keep working.
#[derive(Clone, Debug, Serialize, PartialEq)]
pub struct Alert {
    pub session: Option<String>,
    pub pid: Option<u32>,
    #[serde(rename = "type")]
    pub kind: String,
    pub gravite: u8,
    pub valeur: f64,
    pub resume: String,
    pub processus: Vec<Top>,
}

fn round2(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

fn top3(procs: &[TreeProc]) -> Vec<Top> {
    procs
        .iter()
        .take(3)
        .map(|p| Top { pid: p.pid, go: round2(p.ram as f64 / GB), nom: p.name.clone() })
        .collect()
}

fn alert(tree: Option<&Tree>, kind: &str, gravity: u8, value: f64, summary: String, procs: &[TreeProc]) -> Alert {
    Alert {
        session: tree.map(|t| t.session.name.clone()),
        pid: tree.map(|t| t.session.pid),
        kind: kind.to_string(),
        gravite: gravity,
        valeur: round2(value),
        resume: summary,
        processus: top3(procs),
    }
}

pub fn memory_alerts(snap: &Snapshot, limits: &Thresholds, fr: bool) -> Vec<Alert> {
    let mut out = Vec::new();
    let (tree_alert, tree_critical) = (limits.session_gb * GB, limits.session_critical_gb * GB);
    for tree in &snap.trees {
        let ram = tree.ram as f64;
        if ram >= tree_alert {
            let gravity = if ram >= tree_critical { 2 } else { 1 };
            let limit = gb(if gravity == 2 { tree_critical } else { tree_alert }, fr);
            let size = gb(ram, fr);
            out.push(alert(Some(tree), "ram_session", gravity, ram / GB,
                tr!(fr, "arbre de {size} Go (seuil {limit} Go)", "process tree of {size} GB (threshold {limit} GB)"), &tree.procs));
        }
    }
    let heavy: Vec<&Tree> = snap.trees.iter().filter(|t| t.ram > 0).take(SESSIONS_UNDER_PRESSURE).collect();
    let total = gb(snap.memtotal as f64, fr);
    let weighs = |tree: Option<&Tree>| match tree {
        Some(t) => {
            let size = gb(t.ram as f64, fr);
            tr!(fr, "cette session pèse {size} Go", "this session weighs {size} GB")
        }
        None => tr!(fr, "aucune session en cause", "no session to blame"),
    };
    let targets: Vec<Option<&Tree>> = if heavy.is_empty() { vec![None] } else { heavy.iter().copied().map(Some).collect() };
    if snap.pressure >= 2 {
        let gravity = if snap.pressure >= 4 { 2 } else { 1 };
        let level = if gravity == 2 { tr!(fr, "critique", "critical") } else { tr!(fr, "en alerte", "warning") };
        for &tree in &targets {
            let who = weighs(tree);
            let summary = tr!(fr, "pression mémoire {level} sur {total} Go ; {who}", "memory pressure {level} on {total} GB; {who}");
            let value = tree.map_or(snap.pressure as f64, |t| t.ram as f64 / GB);
            let procs = tree.map_or(snap.top_system.as_slice(), |t| t.procs.as_slice());
            out.push(alert(tree, "pression", gravity, value, summary, procs));
        }
    }
    if snap.swap as f64 >= limits.swap_gb * GB {
        let gravity = if snap.swap as f64 >= limits.swap_critical_gb * GB { 2 } else { 1 };
        let (used, limit) = (gb(snap.swap as f64, fr), gb(limits.swap_gb * GB, fr));
        for &tree in &targets {
            let who = weighs(tree);
            let summary = tr!(fr, "swap à {used} Go (seuil {limit} Go) ; {who}", "swap at {used} GB (threshold {limit} GB); {who}");
            let value = tree.map_or(snap.swap as f64 / GB, |t| t.ram as f64 / GB);
            let procs = tree.map_or(snap.top_system.as_slice(), |t| t.procs.as_slice());
            out.push(alert(tree, "swap", gravity, value, summary, procs));
        }
    }
    out
}

/// The largest drop of free space over the last 15 minutes (bytes, ≥ 0).
pub fn disk_drop(history: &[(f64, u64)], free: u64, at: f64) -> u64 {
    history
        .iter()
        .filter(|(t, _)| at - t <= 15.0 * 60.0)
        .map(|(_, f)| *f)
        .max()
        .unwrap_or(free)
        .saturating_sub(free)
}

pub fn disk_alerts(snap: &Snapshot, history: &[(f64, u64)], growths: &HashMap<String, i64>, at: f64, limits: &Thresholds, fr: bool) -> Vec<Alert> {
    let free = snap.free;
    let drop = disk_drop(history, free, at);
    let free_text = gb(free as f64, fr);
    let mut causes: Vec<(&str, u8, String, f64)> = Vec::new();
    if (free as f64) < limits.disk_free_gb * GB {
        let gravity = if (free as f64) < limits.disk_free_critical_gb * GB { 2 } else { 1 };
        let limit = gb(limits.disk_free_gb * GB, fr);
        causes.push(("disque_bas", gravity, tr!(fr, "{free_text} Go libres (seuil {limit} Go)", "{free_text} GB free (threshold {limit} GB)"),
            limits.disk_free_gb - free as f64 / GB + 1.0));
    }
    if drop as f64 >= limits.disk_drop_gb * GB {
        let lost = gb(drop as f64, fr);
        causes.push(("disque_chute", 1, tr!(fr, "chute de {lost} Go en 15 min, {free_text} Go libres", "dropped {lost} GB in 15 min, {free_text} GB free"), drop as f64 / GB));
    }
    // One culprit per folder: sessions sharing a cwd are named together.
    let mut culprits: BTreeMap<&str, Vec<&Tree>> = BTreeMap::new();
    for tree in &snap.trees {
        if growths.get(&tree.session.cwd).is_some_and(|g| *g as f64 >= limits.folder_growth_gb * GB) {
            culprits.entry(tree.session.cwd.as_str()).or_default().push(tree);
        }
    }
    let mut out = Vec::new();
    for (kind, gravity, text, value) in causes {
        // A durably low disk is a state, not an event: only a drop names someone.
        if kind == "disque_bas" || culprits.is_empty() {
            let rest = if kind == "disque_bas" {
                tr!(fr, "état durable, aucune session désignée", "lasting state, no session named")
            } else if growths.is_empty() {
                tr!(fr, "cause non mesurée (pas encore deux mesures du)", "cause not measured (not two du readings yet)")
            } else {
                tr!(fr, "sans coupable", "no culprit")
            };
            out.push(alert(None, kind, gravity, value, format!("{text} ; {rest}"), &snap.top_system));
            continue;
        }
        for (cwd, group) in &culprits {
            let grew = gb(growths[*cwd] as f64, fr);
            let shared = if group.len() > 1 {
                let others = group[1..].iter().map(|t| t.session.name.as_str()).collect::<Vec<_>>().join(", ");
                tr!(fr, " (dossier partagé avec {others})", " (folder shared with {others})")
            } else {
                String::new()
            };
            let summary = tr!(fr, "{text} ; son dossier {cwd} a grossi de {grew} Go{shared} (mesure du, les clones APFS la surestiment)",
                "{text}; its folder {cwd} grew by {grew} GB{shared} (du reading, APFS clones overstate it)");
            out.push(alert(Some(group[0]), kind, gravity, growths[*cwd] as f64 / GB, summary, &group[0].procs));
        }
    }
    out
}

/// What was last said per "session|type".
#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
pub struct Said {
    pub t: f64,
    pub gravite: u8,
    pub valeur: f64,
}

/// No new alert for 20 minutes, unless more severe or the value ×1.5.
pub fn filter(alerts: Vec<Alert>, memory: &mut BTreeMap<String, Said>, at: f64) -> Vec<Alert> {
    let mut emitted = Vec::new();
    for alert in alerts {
        let key = format!("{}|{}", alert.session.as_deref().unwrap_or("-"), alert.kind);
        let fresh = match memory.get(&key) {
            None => true,
            Some(before) => {
                at - before.t >= RENEWAL
                    || alert.gravite > before.gravite
                    || (before.valeur > 0.0 && alert.valeur >= before.valeur * MULTIPLIER)
            }
        };
        if fresh {
            memory.insert(key, Said { t: at, gravite: alert.gravite, valeur: alert.valeur });
            emitted.push(alert);
        }
    }
    memory.retain(|_, said| at - said.t <= 24.0 * 3600.0);
    emitted
}

/// {folder: bytes gained} between the oldest and the latest reading of the
/// last 30 minutes.
pub fn growths(du: &BTreeMap<String, Vec<(f64, u64)>>, at: f64) -> HashMap<String, i64> {
    du.iter()
        .filter_map(|(folder, points)| {
            let recent: Vec<&(f64, u64)> = points.iter().filter(|(t, _)| at - t <= DISK_HISTORY).collect();
            (recent.len() >= 2).then(|| (folder.clone(), (recent[recent.len() - 1].1 as i64 - recent[0].1 as i64) * 1024))
        })
        .collect()
}

/// Folders worth a `du`: not one holding other sessions' cwd, nor HOME or /.
pub fn measurable_folders(sessions: &[Session], home: &str) -> Vec<String> {
    let cwds: std::collections::BTreeSet<String> = sessions
        .iter()
        .map(|s| {
            let cwd = s.cwd.trim_end_matches('/');
            if cwd.is_empty() { "/".to_string() } else { cwd.to_string() }
        })
        .collect();
    cwds.iter()
        .filter(|c| c.as_str() != "/" && c.as_str() != home.trim_end_matches('/'))
        .filter(|c| !cwds.iter().any(|o| o != *c && o.starts_with(&format!("{c}/"))))
        .cloned()
        .collect()
}

pub fn format_alerts(alerts: &[Alert], fr: bool) -> String {
    let mut lines = Vec::new();
    for alert in alerts {
        let procs = alert
            .processus
            .iter()
            .map(|p| {
                let size = if fr { p.go.to_string().replace('.', ",") } else { p.go.to_string() };
                let (pid, name) = (p.pid, &p.nom);
                tr!(fr, "{pid} {size} Go {name}", "{pid} {size} GB {name}")
            })
            .collect::<Vec<_>>()
            .join("; ");
        let level = match (alert.gravite, fr) {
            (2, true) => "CRITIQUE",
            (2, false) => "CRITICAL",
            (_, true) => "ALERTE",
            (_, false) => "WARNING",
        };
        let session = alert.session.clone().unwrap_or_else(|| tr!(fr, "aucune", "none"));
        let procs = if procs.is_empty() { tr!(fr, "aucun", "none") } else { procs };
        let (kind, summary) = (&alert.kind, &alert.resume);
        lines.push(tr!(fr, "[{level}] session {session} | {kind} | {summary} | plus gros processus : {procs}",
            "[{level}] session {session} | {kind} | {summary} | biggest processes: {procs}"));
    }
    lines.push(json!({ "alertes": alerts }).to_string());
    lines.join("\n")
}

// Persistent state (anti-repeat, history).

#[derive(Serialize, Deserialize, Default)]
pub struct State {
    #[serde(default)]
    pub alertes: BTreeMap<String, Said>,
    #[serde(default)]
    pub disque: Vec<(f64, u64)>,
    #[serde(default)]
    pub du: BTreeMap<String, Vec<(f64, u64)>>,
    #[serde(default)]
    pub essais: BTreeMap<String, f64>,
}

fn state_file() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join("AgentsSleepPreventer")
        .join("monitor.json")
}

fn load_state() -> State {
    std::fs::read_to_string(state_file())
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

fn save_state(state: &State) {
    let file = state_file();
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let temporary = file.with_extension(format!("{}.tmp", std::process::id()));
    if let Ok(text) = serde_json::to_string(state) {
        if std::fs::write(&temporary, text).is_ok() {
            let _ = std::fs::rename(&temporary, &file);
        }
    }
}

/// `du -sk` of session folders in a thread of its own: the watch never waits
/// for it. A `du` runs niced, in its own process group, killed at timeout.
/// `du -sk` readings per folder: (when, kilobytes).
type Readings = BTreeMap<String, Vec<(f64, u64)>>;

struct Measurer {
    queue: Arc<Mutex<Vec<String>>>,
    readings: Arc<Mutex<Readings>>,
    busy: Arc<Mutex<bool>>,
}

impl Measurer {
    fn new(du: BTreeMap<String, Vec<(f64, u64)>>) -> Self {
        Self {
            queue: Arc::new(Mutex::new(Vec::new())),
            readings: Arc::new(Mutex::new(du)),
            busy: Arc::new(Mutex::new(false)),
        }
    }

    fn plan(&self, folders: Vec<String>, tries: &mut BTreeMap<String, f64>, at: f64) {
        let mut queue = self.queue.lock().unwrap();
        for folder in folders {
            if at - tries.get(&folder).copied().unwrap_or(0.0) >= DU_PERIOD && !queue.contains(&folder) {
                tries.insert(folder.clone(), at); // even a failure isn't retried before 15 min
                queue.push(folder);
            }
        }
        let mut busy = self.busy.lock().unwrap();
        if queue.is_empty() || *busy {
            return;
        }
        *busy = true;
        let (queue, readings, flag) = (self.queue.clone(), self.readings.clone(), self.busy.clone());
        std::thread::spawn(move || loop {
            let next = queue.lock().unwrap().pop();
            let Some(folder) = next else {
                *flag.lock().unwrap() = false;
                return;
            };
            if let Some(kilobytes) = measure(&folder) {
                readings.lock().unwrap().entry(folder).or_default().push((now(), kilobytes));
            }
        });
    }

    fn copy(&self, at: f64) -> BTreeMap<String, Vec<(f64, u64)>> {
        let mut readings = self.readings.lock().unwrap();
        for points in readings.values_mut() {
            points.retain(|(t, _)| at - t <= DU_KEEP);
        }
        readings.retain(|_, points| !points.is_empty());
        readings.clone()
    }
}

fn measure(folder: &str) -> Option<u64> {
    use std::os::unix::process::CommandExt;
    let mut child = Command::new("/usr/bin/nice")
        .args(["-n", "19", "/usr/bin/du", "-sk", folder])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .ok()?;
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if started.elapsed() > DU_TIMEOUT => {
                unsafe { libc::killpg(child.id() as libc::pid_t, libc::SIGKILL) };
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(200)),
            Err(_) => return None,
        }
    }
    let mut output = String::new();
    std::io::Read::read_to_string(child.stdout.as_mut()?, &mut output).ok()?;
    output.lines().last()?.split('\t').next()?.trim().parse().ok()
}

/// The watch: one reading every 15 s, alerts filtered against repeats.
pub struct Watch {
    state: State,
    measurer: Measurer,
    pub limits: Thresholds,
    pub fr: bool,
}

impl Watch {
    pub fn new() -> Self {
        let mut state = load_state();
        let du = std::mem::take(&mut state.du);
        for (folder, points) in &du {
            if let Some((t, _)) = points.last() {
                let tried = state.essais.entry(folder.clone()).or_insert(0.0);
                *tried = tried.max(*t);
            }
        }
        Self {
            state,
            measurer: Measurer::new(du),
            limits: thresholds(),
            fr: crate::telegram::owner_prefers_french(),
        }
    }

    /// One turn: history, du readings, evaluation, anti-repeat; the state is saved.
    pub fn cycle(&mut self, snap: &Snapshot) -> Vec<Alert> {
        let at = now();
        self.state.disque.retain(|(t, _)| at - t <= DISK_HISTORY);
        self.state.essais.retain(|_, t| at - *t <= 24.0 * 3600.0);
        self.measurer.plan(measurable_folders(&snap.sessions, &snap.home), &mut self.state.essais, at);
        let du = self.measurer.copy(at);
        let mut alerts = memory_alerts(snap, &self.limits, self.fr);
        alerts.extend(disk_alerts(snap, &self.state.disque, &growths(&du, at), at, &self.limits, self.fr));
        self.state.disque.push((at, snap.free));
        let emitted = filter(alerts, &mut self.state.alertes, at);
        self.state.du = du;
        save_state(&self.state);
        self.state.du.clear();
        emitted
    }

    pub fn history(&self) -> &[(f64, u64)] {
        &self.state.disque
    }
}

/// The Telegram session listening inside a Claude Code session's process
/// tree (`… telegram listen --session NAME`), if any.
fn telegram_listener(session: u32, table: &[crate::ProcessInfo]) -> Option<String> {
    let parents: HashMap<u32, u32> = table.iter().map(|p| (p.pid, p.ppid)).collect();
    let inside = |mut pid: u32| {
        let mut steps = 0;
        while pid > 1 && steps < 64 {
            if pid == session {
                return true;
            }
            pid = parents.get(&pid).copied().unwrap_or(0);
            steps += 1;
        }
        false
    };
    table
        .iter()
        .filter(|p| p.args.contains("telegram listen") || p.args.contains("bot listen"))
        .filter(|p| inside(p.pid))
        .find_map(|p| {
            let mut words = p.args.split_whitespace();
            while let Some(word) = words.next() {
                if word == "--session" {
                    return words.next().map(str::to_string);
                }
                if let Some(name) = word.strip_prefix("--session=") {
                    return Some(name.to_string());
                }
            }
            None
        })
}

fn kind_label(kind: &str, fr: bool) -> String {
    match kind {
        "ram_session" => tr!(fr, "mémoire de ta session", "your session's memory"),
        "pression" => tr!(fr, "pression mémoire", "memory pressure"),
        "swap" => tr!(fr, "swap", "swap"),
        "disque_bas" => tr!(fr, "disque presque plein", "disk almost full"),
        "disque_chute" => tr!(fr, "disque qui fond", "disk emptying fast"),
        other => other.to_string(),
    }
}

/// What the culprit session reads: the problem first, the figures, what
/// is expected of it, and where it comes from.
fn session_message(alert: &Alert, owner: &str, fr: bool) -> String {
    let level = if alert.gravite == 2 { tr!(fr, "critique", "critical") } else { tr!(fr, "alerte", "warning") };
    let (label, summary) = (kind_label(&alert.kind, fr), &alert.resume);
    let procs = alert
        .processus
        .iter()
        .map(|p| {
            let size = if fr { p.go.to_string().replace('.', ",") } else { p.go.to_string() };
            let (pid, name) = (p.pid, &p.nom);
            tr!(fr, "{name} (pid {pid}, {size} Go)", "{name} (pid {pid}, {size} GB)")
        })
        .collect::<Vec<_>>()
        .join(", ");
    let advice = if alert.kind.starts_with("disque") {
        tr!(fr, "Regarde ce que ce dossier a écrit récemment (build, cache, téléchargement, logs) et supprime ce qui ne sert plus. du surestime les clones APFS : seule la variation compte.",
            "Look at what this folder wrote recently (build, cache, download, logs) and delete what is no longer needed. du overstates APFS clones: only the change matters.")
    } else {
        tr!(fr, "Regarde ce qui consomme et arrête ce qui ne sert plus (serveur de dev oublié, build, téléchargement).",
            "Look at what uses it and stop what is no longer needed (a forgotten dev server, a build, a download).")
    };
    tr!(fr,
        "{label} ({level}) : {summary}\nPlus gros processus : {procs}\n{advice} Puis dis en une ligne à {owner} ce que tu as fait, ou pourquoi tu gardes tout.\n(Mesure d’ASP sur la machine, pas un message de {owner}.)",
        "{label} ({level}): {summary}\nBiggest processes: {procs}\n{advice} Then tell {owner} in one line what you did, or why you keep everything.\n(A measurement by ASP on this machine, not a message from {owner}.)")
}

/// Hands the alerts out: a notification, the culprit sessions' Telegram
/// inboxes, and the owner when nobody can be told or it is critical.
fn deliver(alerts: &[Alert], fr: bool) {
    let Some(first) = alerts.first() else {
        return;
    };
    let label = kind_label(&first.kind, fr);
    let title = tr!(fr, "ASP : {label}", "ASP: {label}");
    let more = if alerts.len() > 1 { format!(" (+{})", alerts.len() - 1) } else { String::new() };
    crate::notifications::spool(&title, &format!("{}{more}", first.resume), first.pid);
    let Some(bot) = crate::telegram::configured_bot() else {
        return;
    };
    let owner = bot.owner();
    let table = crate::load_process_table();
    let mut for_owner = Vec::new();
    for alert in alerts {
        let shared = matches!(alert.kind.as_str(), "pression" | "swap");
        let worth = !shared || alert.valeur >= 1.0;
        let listener = alert
            .pid
            .filter(|_| worth && !alert.session.as_deref().is_some_and(|s| s.starts_with("pid-")))
            .and_then(|pid| telegram_listener(pid, &table));
        let told = listener.is_some_and(|name| {
            bot.drop_in_inbox(&name, "ASP", &session_message(alert, &owner, fr)).is_ok()
        });
        if !told || alert.gravite == 2 || alert.session.is_none() {
            for_owner.push(alert);
        }
    }
    if for_owner.is_empty() {
        return;
    }
    let lines = for_owner
        .iter()
        .map(|a| {
            let session = a.session.clone().unwrap_or_else(|| tr!(fr, "aucune session", "no session"));
            let (label, summary) = (kind_label(&a.kind, fr), &a.resume);
            format!("• {label} — {session} : {summary}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let name = tr!(fr, "moniteur", "monitor");
    let _ = bot.send(&lines, &name, true);
}

/// The agent's watch, in a thread of its own so dictation never waits.
pub fn spawn() {
    if !thresholds().enabled {
        return;
    }
    std::thread::spawn(|| {
        let mut watch = Watch::new();
        loop {
            let alerts = watch.cycle(&read_system());
            if !alerts.is_empty() {
                crate::logging::log(&format!("[monitor] {} alert(s): {}", alerts.len(), alerts[0].resume));
                deliver(&alerts, watch.fr);
            }
            std::thread::sleep(Duration::from_secs(STEP));
        }
    });
}

/// `asp monitor watch`: exits 0 as soon as there are alerts (printed), 3
/// when `max` seconds pass without any.
pub fn watch(max: Option<u64>) -> i32 {
    let started = Instant::now();
    let mut watch = Watch::new();
    loop {
        let alerts = watch.cycle(&read_system());
        if !alerts.is_empty() {
            println!("{}", format_alerts(&alerts, watch.fr));
            return 0;
        }
        let left = max.map(|max| max.saturating_sub(started.elapsed().as_secs()));
        if left == Some(0) {
            return 3;
        }
        std::thread::sleep(Duration::from_secs(left.map_or(STEP, |l| l.min(STEP))));
    }
}

/// `asp monitor status`: the system, then each session's RAM.
pub fn status() -> String {
    let watch = Watch::new();
    let fr = watch.fr;
    let snap = read_system();
    let drop = disk_drop(watch.history(), snap.free, now());
    let pressure = match (snap.pressure, fr) {
        (1, true) => "normale".to_string(),
        (2, true) => "alerte".to_string(),
        (4, true) => "critique".to_string(),
        (1, false) => "normal".to_string(),
        (2, false) => "warning".to_string(),
        (4, false) => "critical".to_string(),
        (other, _) => other.to_string(),
    };
    let (total, swap, free, outside) = (gb(snap.memtotal as f64, fr), gb(snap.swap as f64, fr), gb(snap.free as f64, fr), gb(snap.outside as f64, fr));
    let mut lines = vec![
        tr!(fr, "SYSTÈME", "SYSTEM"),
        tr!(fr, "  RAM totale        {total} Go", "  total RAM         {total} GB"),
        tr!(fr, "  pression mémoire  {pressure}", "  memory pressure   {pressure}"),
        tr!(fr, "  swap utilisé      {swap} Go", "  swap used         {swap} GB"),
        tr!(fr, "  disque libre      {free} Go", "  free disk         {free} GB"),
        tr!(fr, "  hors sessions     {outside} Go de RAM", "  outside sessions  {outside} GB of RAM"),
        String::new(),
        tr!(fr, "SESSIONS (RAM de l'arbre de processus)", "SESSIONS (RAM of the process tree)"),
    ];
    if drop > 0 {
        let lost = gb(drop as f64, fr);
        lines.insert(5, tr!(fr, "  chute récente     {lost} Go en 15 min", "  recent drop       {lost} GB in 15 min"));
    }
    for tree in &snap.trees {
        let size = gb(tree.ram as f64, fr);
        let unit = if fr { "Go" } else { "GB" };
        lines.push(format!("  {:<22} pid {:<7} {:>6} {unit}  {}", tree.session.name, tree.session.pid, size, tree.session.cwd));
        let procs = tree
            .procs
            .iter()
            .take(3)
            .map(|p| {
                let orphan = if p.orphan { tr!(fr, " (orphelin)", " (orphan)") } else { String::new() };
                format!("{} {} {} {unit}{orphan}", p.name, p.pid, gb(p.ram as f64, fr))
            })
            .collect::<Vec<_>>()
            .join(", ");
        if !procs.is_empty() {
            lines.push(format!("      {procs}"));
        }
    }
    if snap.trees.is_empty() {
        lines.push(tr!(fr, "  aucune session vivante", "  no live session"));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(pid: u32, name: &str, cwd: &str) -> Session {
        Session { pid, name: name.to_string(), cwd: cwd.to_string() }
    }

    fn procs(list: &[(u32, u32, u64, &str)]) -> HashMap<u32, Proc> {
        list.iter()
            .map(|(pid, ppid, gb, command)| (*pid, Proc { ppid: *ppid, ram: gb * GIB, command: command.to_string() }))
            .collect()
    }

    fn snapshot(trees: Vec<Tree>) -> Snapshot {
        Snapshot { trees, top_system: Vec::new(), outside: 0, pressure: 1, swap: 0, memtotal: 64 * GIB, free: 200 * GIB, sessions: Vec::new(), home: "/Users/x".into() }
    }

    #[test]
    fn processes_go_to_their_nearest_session_and_big_orphans_by_folder() {
        let table = procs(&[
            (100, 1, 1, "/usr/bin/claude"),
            (101, 100, 3, "node"),
            (102, 101, 2, "esbuild"),
            (200, 1, 2, "/Users/x/.local/bin/server"),
            (201, 200, 1, "worker"),
            (300, 1, 5, "/System/Library/WindowServer"),
        ]);
        let sessions = vec![session(100, "dev", "/Users/x/projects/app")];
        let (trees, others) = attribute(&table, &sessions, |pid| (pid == 200).then(|| "/Users/x/projects/app/api".to_string()));
        assert_eq!(trees[0].ram, 9 * GIB);
        assert_eq!(trees[0].procs.iter().filter(|p| p.orphan).count(), 2);
        assert_eq!(others.iter().map(|p| p.pid).collect::<Vec<_>>(), [300]);
    }

    #[test]
    fn ghost_sessions_with_a_reused_pid_are_ignored() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("10.json"), r#"{"pid": 10, "cwd": "/a", "name": "vivante", "procStart": "Sun Nov  1 05:08:55 2026"}"#).unwrap();
        std::fs::write(dir.path().join("11.json"), r#"{"pid": 11, "cwd": "/b", "procStart": "Sat Oct 31 01:00:00 2026"}"#).unwrap();
        std::fs::write(dir.path().join("12.json"), r#"{"pid": 12, "cwd": "/c"}"#).unwrap();
        let alive = HashSet::from([10, 11]);
        let starts = HashMap::from([(10, "Sun Nov 1 05:08:55 2026".to_string()), (11, "Sun Nov 1 06:00:00 2026".to_string())]);
        let folders = vec![dir.path().to_path_buf(), dir.path().to_path_buf()];
        assert_eq!(read_sessions(&folders, &alive, &starts), vec![session(10, "vivante", "/a")]);
    }

    #[test]
    fn start_times_read_like_claude_writes_them() {
        assert_eq!(start_text(1793509735), "Sun Nov 1 05:08:55 2026");
    }

    #[test]
    fn a_heavy_tree_is_an_alert_then_critical_and_not_repeated() {
        let limits = Thresholds::default();
        let tree = |gb: u64| Tree { session: session(1, "dev", "/p"), ram: gb * GIB, procs: vec![TreeProc { pid: 2, ram: gb * GIB, name: "node".into(), orphan: false }] };
        let alerts = memory_alerts(&snapshot(vec![tree(9)]), &limits, true);
        assert_eq!((alerts[0].kind.as_str(), alerts[0].gravite), ("ram_session", 1));
        assert!(alerts[0].resume.contains("9,0 Go"));
        let mut memory = BTreeMap::new();
        assert_eq!(filter(alerts.clone(), &mut memory, 1000.0).len(), 1);
        assert!(filter(alerts.clone(), &mut memory, 1100.0).is_empty());
        let critical = memory_alerts(&snapshot(vec![tree(17)]), &limits, true);
        assert_eq!(critical[0].gravite, 2);
        assert_eq!(filter(critical, &mut memory, 1200.0).len(), 1);
        assert_eq!(filter(alerts, &mut memory, 1000.0 + RENEWAL + 300.0).len(), 1);
    }

    #[test]
    fn a_disk_drop_blames_the_folder_that_grew() {
        let limits = Thresholds::default();
        let mut snap = snapshot(vec![Tree { session: session(1, "dev", "/p"), ram: GIB, procs: Vec::new() }]);
        snap.free = 100 * GIB;
        let history = vec![(0.0, 115 * GIB)];
        let grew = HashMap::from([("/p".to_string(), (12 * GIB) as i64)]);
        let alerts = disk_alerts(&snap, &history, &grew, 600.0, &limits, false);
        assert_eq!((alerts[0].kind.as_str(), alerts[0].session.as_deref()), ("disque_chute", Some("dev")));
        let unknown = disk_alerts(&snap, &history, &HashMap::new(), 600.0, &limits, false);
        assert!(unknown[0].resume.contains("not measured"));
    }

    #[test]
    fn du_skips_home_and_folders_holding_other_sessions() {
        let sessions = vec![session(1, "a", "/Users/x/p"), session(2, "b", "/Users/x/p/sub"), session(3, "c", "/Users/x")];
        assert_eq!(measurable_folders(&sessions, "/Users/x"), ["/Users/x/p/sub"]);
    }

    #[test]
    fn alerts_print_one_line_each_then_json() {
        let alert = Alert { session: Some("dev".into()), pid: Some(1), kind: "ram_session".into(), gravite: 2, valeur: 17.2, resume: "arbre de 17,2 Go".into(), processus: vec![Top { pid: 2, go: 12.5, nom: "node".into() }] };
        let text = format_alerts(&[alert], true);
        let mut lines = text.lines();
        assert_eq!(lines.next().unwrap(), "[CRITIQUE] session dev | ram_session | arbre de 17,2 Go | plus gros processus : 2 12,5 Go node");
        let parsed: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        assert_eq!(parsed["alertes"][0]["processus"][0]["go"], 12.5);
    }

    fn process(pid: u32, ppid: u32, args: &str) -> crate::ProcessInfo {
        crate::ProcessInfo { pid, ppid, arg0: args.split(' ').next().unwrap().to_string(), args: args.to_string() }
    }

    #[test]
    fn the_listener_inside_a_session_names_its_telegram_session() {
        let table = vec![
            process(100, 1, "claude"),
            process(101, 100, "/bin/zsh -c eval 'asp telegram listen --session dev-asp --max 0'"),
            process(102, 101, "/usr/local/bin/asp telegram listen --session dev-asp --max 0"),
            process(200, 1, "claude"),
            process(201, 200, "python -m sync_telegram.cli bot listen --session=autre --max 0"),
        ];
        assert_eq!(telegram_listener(100, &table).as_deref(), Some("dev-asp"));
        assert_eq!(telegram_listener(200, &table).as_deref(), Some("autre"));
        assert_eq!(telegram_listener(300, &table), None);
    }

    #[test]
    fn the_session_reads_the_problem_first() {
        let alert = Alert { session: Some("dev".into()), pid: Some(1), kind: "ram_session".into(), gravite: 2, valeur: 17.2,
            resume: "arbre de 17,2 Go (seuil 16,0 Go)".into(), processus: vec![Top { pid: 2, go: 12.5, nom: "node".into() }] };
        let text = session_message(&alert, "Charles", true);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], "mémoire de ta session (critique) : arbre de 17,2 Go (seuil 16,0 Go)");
        assert_eq!(lines[1], "Plus gros processus : node (pid 2, 12,5 Go)");
        assert!(lines[3].contains("pas un message de Charles"));
    }

    /// Drops a real alert in the inbox of the Telegram session listening in
    /// `ASP_MONITOR_SESSION_PID`'s tree:
    /// `ASP_MONITOR_SESSION_PID=1234 cargo test real_delivery -- --ignored`
    #[test]
    #[ignore]
    fn real_delivery() {
        let pid: u32 = std::env::var("ASP_MONITOR_SESSION_PID").unwrap().parse().unwrap();
        let alert = Alert { session: Some("test".into()), pid: Some(pid), kind: "ram_session".into(), gravite: 1, valeur: 9.0,
            resume: "TEST : arbre de 9,0 Go (seuil 8,0 Go)".into(), processus: vec![Top { pid, go: 9.0, nom: "claude".into() }] };
        deliver(&[alert], true);
    }
}
