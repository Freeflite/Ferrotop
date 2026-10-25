//! Find handles, DLLs or windows: searches every process's loaded modules, named handles and
//! top-level windows.

use std::collections::HashMap;

use ferrotop_sys::desktop::top_level_windows;
use ferrotop_sys::handles::handles;
use ferrotop_sys::modules::modules;

use crate::properties::stable_id;
use crate::table::{Cell, Column, Table, Tone};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    pub pid: u32,
    pub process: String,
    /// `DLL` for a module, `Window` for a window, else the handle's object type.
    pub kind: String,
    pub name: String,
    /// The handle, or a window's handle.
    pub handle: Option<u64>,
}

/// Searches `processes` (`(pid, name)`) for windows whose title or class, and modules and
/// handles whose name or path, contains `query` (case-insensitive). `progress` is called with
/// (done, total) after each process; it returns `false` to cancel. Processes Ferrotop can't
/// open are skipped.
pub fn search(query: &str, processes: &[(u32, String)], mut progress: impl FnMut(usize, usize) -> bool) -> Vec<Match> {
    let needle = query.trim().to_lowercase();
    let mut matches = Vec::new();
    if needle.is_empty() {
        return matches;
    }
    // Windows are all listed in one call, so they come first.
    let names: HashMap<u32, &String> = processes.iter().map(|(pid, name)| (*pid, name)).collect();
    for window in top_level_windows().unwrap_or_default() {
        let Some(process) = names.get(&window.pid) else { continue };
        if !(window.title.to_lowercase().contains(&needle) || window.class.to_lowercase().contains(&needle)) {
            continue;
        }
        let title = if window.title.is_empty() { "(untitled)" } else { &window.title };
        matches.push(Match {
            pid: window.pid,
            process: (*process).clone(),
            kind: "Window".into(),
            name: format!("{title}  ({})", window.class),
            handle: Some(window.handle as u64),
        });
    }
    for (done, (pid, process)) in processes.iter().enumerate() {
        if *pid == 0 {
            continue;
        }
        if let Ok(modules) = modules(*pid) {
            for module in modules.iter().filter(|m| m.path.to_lowercase().contains(&needle)) {
                matches.push(Match {
                    pid: *pid,
                    process: process.clone(),
                    kind: "DLL".into(),
                    name: module.path.clone(),
                    handle: None,
                });
            }
        }
        if let Ok(handles) = handles(*pid) {
            for handle in handles {
                if let Some(name) = handle.name.filter(|n| n.to_lowercase().contains(&needle)) {
                    matches.push(Match {
                        pid: *pid,
                        process: process.clone(),
                        kind: handle.type_name,
                        name,
                        handle: Some(handle.handle),
                    });
                }
            }
        }
        if !progress(done + 1, processes.len()) {
            break;
        }
    }
    matches
}

pub fn matches_table(matches: &[Match]) -> Table {
    let mut table = Table::new(&[
        Column::text("Process", 190),
        Column::number("PID", 70),
        Column::text("Type", 110),
        Column::text("Name", 520),
        Column::number("Handle", 90),
    ]);
    for m in matches {
        let key = format!("{}|{}|{}|{:?}", m.pid, m.kind, m.name, m.handle);
        table.push(
            stable_id(&key),
            vec![
                Cell::text(&m.process),
                Cell::number(f64::from(m.pid), m.pid.to_string()),
                Cell::text(&m.kind),
                Cell::text(&m.name),
                m.handle.map_or_else(|| Cell::text(""), Cell::hex),
            ],
            Tone::Plain,
        );
    }
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_ntdll_in_this_process() {
        let me = vec![(std::process::id(), "self".to_owned())];
        let mut calls = 0;
        let found = search("NTDLL.DLL", &me, |done, total| {
            calls += 1;
            assert_eq!((done, total), (1, 1));
            true
        });
        assert_eq!(calls, 1);
        assert!(found.iter().any(|m| m.kind == "DLL" && m.name.to_lowercase().ends_with("ntdll.dll")));
        assert!(search("   ", &me, |_, _| true).is_empty());
        assert_eq!(matches_table(&found).rows.len(), found.len());
    }
}
