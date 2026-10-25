//! Per-process facts that don't change while a process lives: who it runs as, whether it's
//! elevated, its image path and version resource. Like System Informer's "stage 1" query, these
//! are looked up once per process (on the sampler thread) and cached.

use std::collections::{HashMap, HashSet};
use std::os::windows::io::OwnedHandle;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ferrotop_sys::icon::{IconPixels, file_icon};
use ferrotop_sys::process::{ProcessInfo, cycle_time, open_for_query};
use ferrotop_sys::signature::{Signature, verify};
use ferrotop_sys::token::{Sid, account_name, current_token, process_token};
use ferrotop_sys::version::{VersionInfo, version_info};

use crate::provider::ProcessKey;

/// How often (in samples) the running-services list is refreshed.
const SERVICES_REFRESH: u32 = 5;
/// Time per sample spent on icons and signature checks, which can take milliseconds per file.
const FILES_BUDGET: Duration = Duration::from_millis(200);
/// Past this many cached files (a long session running many different programs), start over.
const MAX_FILES: usize = 4096;

/// What kind of account a process runs as, for highlighting.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Account {
    /// The token couldn't be read (usually a protected process, or we aren't elevated).
    #[default]
    Unknown,
    /// LocalSystem, LocalService or NetworkService.
    System,
    /// The same user Ferrotop runs as.
    Own,
    Other,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessDetails {
    /// `DOMAIN\name`.
    pub user: Option<String>,
    pub account: Account,
    pub elevated: bool,
    pub image_path: Option<String>,
    pub description: Option<String>,
    pub company: Option<String>,
}

/// Facts about an image file: its icon and whether it's signed. Shared by every process
/// running that file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileFacts {
    pub icon: Option<IconPixels>,
    pub signature: Signature,
}

impl FileFacts {
    /// The verified signer, or a short reason there isn't one.
    pub fn signer(&self) -> String {
        match &self.signature {
            Signature::Verified(signer) if signer.is_empty() => "(verified)".into(),
            Signature::Verified(signer) => signer.clone(),
            Signature::Unsigned => "(not signed)".into(),
            Signature::Untrusted(_) => "(not trusted)".into(),
        }
    }
}

/// The lower-cased path that identifies a file in the facts caches.
pub fn file_key(path: &str) -> String {
    path.to_lowercase()
}

#[derive(Debug)]
pub struct DetailsCollector {
    known: HashMap<ProcessKey, Arc<ProcessDetails>>,
    account_names: HashMap<Sid, Option<String>>,
    versions: HashMap<String, VersionInfo>,
    own_user: Option<Sid>,
    services: Arc<HashMap<u32, Vec<String>>>,
    samples_since_services: u32,
    /// A query handle per process, kept so an exited process's final cycle count can still be
    /// read (System Informer's `QueryHandle`).
    handles: HashMap<ProcessKey, OwnedHandle>,
    /// Processes whose handle couldn't be opened, so they aren't retried every sample.
    unopenable: HashSet<ProcessKey>,
    exited: HashMap<ProcessKey, u64>,
    files: HashMap<String, Arc<FileFacts>>,
    new_files: Vec<(String, Arc<FileFacts>)>,
}

impl Default for DetailsCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl DetailsCollector {
    pub fn new() -> Self {
        Self {
            known: HashMap::new(),
            account_names: HashMap::new(),
            versions: HashMap::new(),
            own_user: current_token().ok().map(|token| token.user),
            services: Arc::default(),
            samples_since_services: SERVICES_REFRESH,
            handles: HashMap::new(),
            unopenable: HashSet::new(),
            exited: HashMap::new(),
            files: HashMap::new(),
            new_files: Vec::new(),
        }
    }

    /// Final cycle counts of processes that exited since the previous sample.
    pub fn take_exited_cycles(&mut self) -> HashMap<ProcessKey, u64> {
        std::mem::take(&mut self.exited)
    }

    /// File facts found since the previous call.
    pub fn take_new_files(&mut self) -> Vec<(String, Arc<FileFacts>)> {
        std::mem::take(&mut self.new_files)
    }

    /// Keeps a query handle on every live process and reads the final cycles of those that exited.
    fn track_handles(&mut self, processes: &[ProcessInfo], live: &HashSet<ProcessKey>) {
        let gone: Vec<ProcessKey> = self.handles.keys().filter(|key| !live.contains(key)).copied().collect();
        for key in gone {
            if let Some(handle) = self.handles.remove(&key)
                && let Ok(cycles) = cycle_time(&handle)
            {
                self.exited.insert(key, cycles);
            }
        }
        self.unopenable.retain(|key| live.contains(key));
        for info in processes.iter().filter(|info| info.pid > 4) {
            let key = ProcessKey { pid: info.pid, create_time: info.create_time };
            if self.handles.contains_key(&key) || self.unopenable.contains(&key) {
                continue;
            }
            match open_for_query(info.pid, info.create_time) {
                Ok(handle) => {
                    self.handles.insert(key, handle);
                }
                Err(_) => {
                    self.unopenable.insert(key);
                }
            }
        }
    }

    /// Icons and signature checks for image files not seen before, within a time budget.
    fn collect_files(&mut self) {
        if self.files.len() > MAX_FILES {
            self.files.clear();
        }
        let started = Instant::now();
        let paths: Vec<String> = self
            .known
            .values()
            .filter_map(|details| details.image_path.clone())
            .filter(|path| !self.files.contains_key(&file_key(path)))
            .collect();
        for path in paths {
            if started.elapsed() > FILES_BUDGET {
                break;
            }
            let key = file_key(&path);
            if self.files.contains_key(&key) {
                continue;
            }
            let facts = Arc::new(FileFacts { icon: file_icon(&path).ok(), signature: verify(&path) });
            self.files.insert(key.clone(), Arc::clone(&facts));
            self.new_files.push((key, facts));
        }
    }

    /// Details for every process in `processes`, looking up new ones until `budget` runs out.
    /// Anything left over is picked up on a later sample.
    pub fn collect(&mut self, processes: &[ProcessInfo], budget: Duration) -> HashMap<ProcessKey, Arc<ProcessDetails>> {
        let started = Instant::now();
        let key = |info: &ProcessInfo| ProcessKey { pid: info.pid, create_time: info.create_time };
        let live: HashSet<ProcessKey> = processes.iter().map(key).collect();
        self.known.retain(|key, _| live.contains(key));
        self.track_handles(processes, &live);
        for info in processes {
            if started.elapsed() > budget {
                break;
            }
            if !self.known.contains_key(&key(info)) {
                let details = Arc::new(self.query(info.pid));
                self.known.insert(key(info), details);
            }
        }
        self.collect_files();
        self.known.clone()
    }

    fn query(&mut self, pid: u32) -> ProcessDetails {
        let system = || ProcessDetails { account: Account::System, ..ProcessDetails::default() };
        match pid {
            0 => ProcessDetails { user: self.account_name(&Sid::local_system()), ..system() },
            // The System process is the kernel; describe it with ntoskrnl's version resource.
            4 => {
                let root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
                let path = format!(r"{root}\System32\ntoskrnl.exe");
                let version = self.version(&path);
                ProcessDetails {
                    user: self.account_name(&Sid::local_system()),
                    image_path: Some(path),
                    description: version.description,
                    company: version.company,
                    ..system()
                }
            }
            pid => {
                let token = process_token(pid).ok();
                let account = match &token {
                    None => Account::Unknown,
                    Some(token) if token.user.is_service_account() => Account::System,
                    Some(token) if Some(&token.user) == self.own_user.as_ref() => Account::Own,
                    Some(_) => Account::Other,
                };
                let user = token.as_ref().and_then(|token| self.account_name(&token.user));
                let image_path = ferrotop_sys::process::image_path(pid).ok();
                let version = image_path.as_deref().map(|path| self.version(path)).unwrap_or_default();
                ProcessDetails {
                    user,
                    account,
                    elevated: token.is_some_and(|token| token.elevated),
                    image_path,
                    description: version.description,
                    company: version.company,
                }
            }
        }
    }

    fn account_name(&mut self, sid: &Sid) -> Option<String> {
        self.account_names.entry(sid.clone()).or_insert_with(|| account_name(sid).ok()).clone()
    }

    fn version(&mut self, path: &str) -> VersionInfo {
        self.versions.entry(path.to_ascii_lowercase()).or_insert_with(|| version_info(path).unwrap_or_default()).clone()
    }

    /// Running services by host PID, refreshed every few samples.
    pub fn services(&mut self) -> Arc<HashMap<u32, Vec<String>>> {
        self.samples_since_services += 1;
        if self.samples_since_services >= SERVICES_REFRESH {
            self.samples_since_services = 0;
            if let Ok(services) = ferrotop_sys::service::running_services() {
                let mut by_pid: HashMap<u32, Vec<String>> = HashMap::new();
                for service in services.into_iter().filter(|s| s.pid != 0) {
                    by_pid.entry(service.pid).or_default().push(service.name);
                }
                for names in by_pid.values_mut() {
                    names.sort_by_key(|name| name.to_ascii_lowercase());
                }
                self.services = Arc::new(by_pid);
            }
        }
        Arc::clone(&self.services)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_this_process_and_the_kernel() {
        let processes = ferrotop_sys::process::enumerate().expect("enumerate");
        let mut collector = DetailsCollector::new();
        let details = collector.collect(&processes, Duration::from_secs(30));

        let me = processes.iter().find(|p| p.pid == std::process::id()).unwrap();
        let mine = &details[&ProcessKey { pid: me.pid, create_time: me.create_time }];
        assert_eq!(mine.account, Account::Own);
        assert!(mine.user.is_some() && mine.image_path.is_some(), "{mine:?}");

        let system = processes.iter().find(|p| p.pid == 4).unwrap();
        let kernel = &details[&ProcessKey { pid: 4, create_time: system.create_time }];
        assert_eq!(kernel.account, Account::System);
        assert!(kernel.company.as_deref().unwrap_or_default().contains("Microsoft"), "{kernel:?}");
    }

    #[test]
    fn reports_final_cycles_of_exited_processes_and_file_facts() {
        let mut collector = DetailsCollector::new();
        let mut child = std::process::Command::new("cmd")
            .args(["/c", "pause"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        let processes = ferrotop_sys::process::enumerate().unwrap();
        let entry = processes.iter().find(|p| p.pid == pid).unwrap().clone();
        // Facts are gathered within a per-sample time budget, so a busy machine needs a few rounds.
        let mut files = Vec::new();
        for _ in 0..100 {
            collector.collect(&processes, Duration::from_secs(30));
            files.extend(collector.take_new_files());
            if files.iter().any(|(path, _)| path.ends_with(r"\cmd.exe")) {
                break;
            }
        }
        assert!(files.iter().any(|(path, facts)| path.ends_with(r"\cmd.exe")
            && matches!(facts.signature, Signature::Verified(_))
            && facts.icon.is_some()));

        let _ = child.kill();
        let _ = child.wait();
        let without_child: Vec<ProcessInfo> = processes.into_iter().filter(|p| p.pid != pid).collect();
        collector.collect(&without_child, Duration::from_secs(30));
        let exited = collector.take_exited_cycles();
        let key = ProcessKey { pid, create_time: entry.create_time };
        assert!(exited.get(&key).is_some_and(|cycles| *cycles >= entry.cycle_time), "{exited:?}");
    }

    #[test]
    fn a_zero_budget_defers_lookups() {
        let processes = ferrotop_sys::process::enumerate().expect("enumerate");
        let mut collector = DetailsCollector::new();
        let first = collector.collect(&processes, Duration::ZERO);
        assert!(first.len() <= 1, "zero budget should look up at most one process, got {}", first.len());
    }

    #[test]
    fn services_map_to_host_processes() {
        let services = DetailsCollector::new().services();
        assert!(services.values().flatten().count() > 20);
    }
}
