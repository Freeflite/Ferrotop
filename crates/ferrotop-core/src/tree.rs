//! Arranges processes into the rows the UI displays: a parent/child tree or a flat list,
//! sorted and filtered.
//!
//! Parent assignment follows System Informer's `proctree.c`: a process is only a child of the
//! process holding its parent PID if that process was created no later than the child. That keeps
//! a recycled PID from adopting processes that belonged to the PID's previous owner.
//!
//! The System process is the exception. Its PID is never recycled, and Windows reports it as
//! created *after* its first children (Secure System, Registry), so the time check would orphan them.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use crate::provider::{ProcessItem, ProcessKey};

/// PID of the System process, which lives for the whole boot.
const SYSTEM_PID: u32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortColumn {
    /// Creation order, as System Informer shows when no column is sorted.
    Natural,
    Name,
    Pid,
    Cpu,
    PrivateBytes,
    WorkingSet,
    IoRate,
    Threads,
    Handles,
    User,
    Description,
    Gpu,
    GpuDedicated,
    GpuShared,
    /// The image file's verified signer.
    Signer,
}

impl SortColumn {
    /// Every column except `Natural`, in display order.
    pub const ALL: [SortColumn; 15] = [
        SortColumn::Name,
        SortColumn::Pid,
        SortColumn::Cpu,
        SortColumn::IoRate,
        SortColumn::PrivateBytes,
        SortColumn::WorkingSet,
        SortColumn::Threads,
        SortColumn::Handles,
        SortColumn::User,
        SortColumn::Description,
        SortColumn::Gpu,
        SortColumn::GpuDedicated,
        SortColumn::GpuShared,
        SortColumn::Signer,
        SortColumn::Natural,
    ];

    /// Stable name used in the settings file.
    pub fn id(self) -> &'static str {
        match self {
            SortColumn::Natural => "natural",
            SortColumn::Name => "name",
            SortColumn::Pid => "pid",
            SortColumn::Cpu => "cpu",
            SortColumn::PrivateBytes => "private-bytes",
            SortColumn::WorkingSet => "working-set",
            SortColumn::IoRate => "io",
            SortColumn::Threads => "threads",
            SortColumn::Handles => "handles",
            SortColumn::User => "user",
            SortColumn::Description => "description",
            SortColumn::Gpu => "gpu",
            SortColumn::GpuDedicated => "gpu-dedicated",
            SortColumn::GpuShared => "gpu-shared",
            SortColumn::Signer => "signer",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|column| column.id() == id)
    }

    /// The order a first click sorts in: text ascending, numbers biggest-first.
    pub fn default_order(self) -> SortOrder {
        match self {
            SortColumn::Natural
            | SortColumn::Name
            | SortColumn::Pid
            | SortColumn::User
            | SortColumn::Description
            | SortColumn::Signer => SortOrder::Ascending,
            _ => SortOrder::Descending,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortOrder {
    Ascending,
    Descending,
}

#[derive(Debug)]
pub struct ViewOptions<'a> {
    pub tree: bool,
    pub sort: SortColumn,
    pub order: SortOrder,
    /// Case-insensitive substring of the name, or a PID prefix. Filtering shows a flat list.
    pub filter: &'a str,
    pub collapsed: &'a HashSet<ProcessKey>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Row {
    pub key: ProcessKey,
    pub depth: u16,
    pub has_children: bool,
    pub expanded: bool,
}

pub fn build_rows<'a>(items: impl IntoIterator<Item = &'a ProcessItem>, options: &ViewOptions<'_>) -> Vec<Row> {
    let mut items: Vec<&ProcessItem> = items.into_iter().collect();
    let compare = |a: &&ProcessItem, b: &&ProcessItem| compare(a, b, options.sort, options.order);
    let flat = |items: Vec<&ProcessItem>| {
        items.into_iter().map(|item| Row { key: item.key, depth: 0, has_children: false, expanded: false }).collect()
    };

    let filter = options.filter.trim();
    if !filter.is_empty() {
        items.retain(|item| matches_filter(item, filter));
        items.sort_by(compare);
        return flat(items);
    }
    if !options.tree {
        items.sort_by(compare);
        return flat(items);
    }

    let parents = assign_parents(&items);
    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut roots = Vec::new();
    for (child, parent) in parents.iter().enumerate() {
        match parent {
            Some(parent) => children.entry(*parent).or_default().push(child),
            None => roots.push(child),
        }
    }
    let by_item = |a: &usize, b: &usize| compare(&items[*a], &items[*b]);
    roots.sort_by(by_item);
    for list in children.values_mut() {
        list.sort_by(by_item);
    }

    let mut rows = Vec::with_capacity(items.len());
    let mut visited = vec![false; items.len()];
    for root in roots {
        push_subtree(root, 0, &items, &children, options.collapsed, &mut visited, &mut rows);
    }
    // Equal creation times could in theory form a parent cycle with no root; show those flat
    // rather than dropping them.
    let mut orphans: Vec<usize> = (0..items.len()).filter(|i| !visited[*i]).collect();
    orphans.sort_by(by_item);
    for orphan in orphans {
        if !visited[orphan] {
            push_subtree(orphan, 0, &items, &children, options.collapsed, &mut visited, &mut rows);
        }
    }
    rows
}

/// For each item, the index of its parent, or `None` if it's a root.
fn assign_parents(items: &[&ProcessItem]) -> Vec<Option<usize>> {
    let mut by_pid: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, item) in items.iter().enumerate() {
        by_pid.entry(item.key.pid).or_default().push(i);
    }
    items
        .iter()
        .map(|child| {
            by_pid
                .get(&child.parent_pid)?
                .iter()
                .copied()
                .filter(|&p| {
                    let parent = items[p].key;
                    parent.pid != child.key.pid
                        && (parent.pid == SYSTEM_PID || parent.create_time <= child.key.create_time)
                })
                .max_by_key(|&p| items[p].key.create_time)
        })
        .collect()
}

fn push_subtree(
    index: usize,
    depth: u16,
    items: &[&ProcessItem],
    children: &HashMap<usize, Vec<usize>>,
    collapsed: &HashSet<ProcessKey>,
    visited: &mut [bool],
    rows: &mut Vec<Row>,
) {
    if visited[index] {
        return;
    }
    visited[index] = true;
    let key = items[index].key;
    let kids = children.get(&index).map_or(&[][..], Vec::as_slice);
    let expanded = !collapsed.contains(&key);
    rows.push(Row { key, depth, has_children: !kids.is_empty(), expanded });
    if expanded {
        for &kid in kids {
            push_subtree(kid, depth.saturating_add(1), items, children, collapsed, visited, rows);
        }
    } else {
        mark_visited(index, children, visited);
    }
}

fn mark_visited(index: usize, children: &HashMap<usize, Vec<usize>>, visited: &mut [bool]) {
    for &kid in children.get(&index).map_or(&[][..], Vec::as_slice) {
        if !visited[kid] {
            visited[kid] = true;
            mark_visited(kid, children, visited);
        }
    }
}

fn matches_filter(item: &ProcessItem, filter: &str) -> bool {
    contains_ignore_ascii_case(&item.name, filter)
        || contains_ignore_ascii_case(item.description(), filter)
        || item.key.pid.to_string().starts_with(filter)
}

fn cmp_ignore_ascii_case(a: &str, b: &str) -> Ordering {
    a.bytes().map(|c| c.to_ascii_lowercase()).cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
}

fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    let needle = needle.as_bytes();
    needle.is_empty() || haystack.as_bytes().windows(needle.len()).any(|window| window.eq_ignore_ascii_case(needle))
}

fn compare(a: &ProcessItem, b: &ProcessItem, sort: SortColumn, order: SortOrder) -> Ordering {
    let primary = match sort {
        SortColumn::Natural => a.key.create_time.cmp(&b.key.create_time),
        SortColumn::Name => cmp_ignore_ascii_case(&a.name, &b.name),
        SortColumn::User => cmp_ignore_ascii_case(a.user(), b.user()),
        SortColumn::Description => cmp_ignore_ascii_case(a.description(), b.description()),
        SortColumn::Gpu => a.gpu.usage.total_cmp(&b.gpu.usage),
        SortColumn::GpuDedicated => a.gpu.dedicated.cmp(&b.gpu.dedicated),
        SortColumn::GpuShared => a.gpu.shared.cmp(&b.gpu.shared),
        SortColumn::Signer => cmp_ignore_ascii_case(&a.signer(), &b.signer()),
        SortColumn::Pid => a.key.pid.cmp(&b.key.pid),
        SortColumn::Cpu => a.cpu_usage.total_cmp(&b.cpu_usage),
        SortColumn::PrivateBytes => a.private_bytes.cmp(&b.private_bytes),
        SortColumn::WorkingSet => a.working_set.cmp(&b.working_set),
        SortColumn::IoRate => a.io_total_rate().total_cmp(&b.io_total_rate()),
        SortColumn::Threads => a.threads.cmp(&b.threads),
        SortColumn::Handles => a.handles.cmp(&b.handles),
    };
    let primary = match order {
        SortOrder::Ascending => primary,
        SortOrder::Descending => primary.reverse(),
    };
    primary.then(a.key.pid.cmp(&b.key.pid)).then(a.key.create_time.cmp(&b.key.create_time))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Instant;

    use ferrotop_sys::process::ProcessInfo;
    use ferrotop_sys::system::{CpuTimes, MemoryStatus};

    use super::*;
    use crate::provider::{ProcessProvider, Sample, is_pseudo_process};

    fn provider(processes: &[(u32, u32, i64, &str)]) -> ProcessProvider {
        let mut provider = ProcessProvider::new();
        provider.update(Sample {
            processes: processes
                .iter()
                .map(|&(pid, parent_pid, create_time, name)| ProcessInfo {
                    pid,
                    parent_pid,
                    create_time,
                    name: name.into(),
                    ..ProcessInfo::default()
                })
                .collect(),
            cpus: vec![CpuTimes::default()],
            cycles: None,
            memory: MemoryStatus::default(),
            details: HashMap::new(),
            services: Arc::default(),
            exited_cycles: HashMap::new(),
            files: Vec::new(),
            taken_at: Instant::now(),
        });
        provider
    }

    /// Rows without the Interrupts pseudo-process the provider always adds.
    fn real(rows: Vec<Row>) -> Vec<Row> {
        rows.into_iter().filter(|r| !is_pseudo_process(r.key.pid)).collect()
    }

    fn options<'a>(collapsed: &'a HashSet<ProcessKey>) -> ViewOptions<'a> {
        ViewOptions { tree: true, sort: SortColumn::Natural, order: SortOrder::Ascending, filter: "", collapsed }
    }

    fn layout(rows: &[Row]) -> Vec<(u32, u16)> {
        rows.iter().map(|r| (r.key.pid, r.depth)).collect()
    }

    #[test]
    fn builds_the_boot_tree() {
        let p = provider(&[
            (0, 0, 0, ""),
            (4, 0, 0, "System"),
            (100, 4, 10, "smss.exe"),
            (200, 999, 20, "wininit.exe"), // parent long gone
            (300, 200, 30, "services.exe"),
        ]);
        let none = HashSet::new();
        let rows = real(build_rows(p.items(), &options(&none)));
        assert_eq!(layout(&rows), vec![(0, 0), (4, 1), (100, 2), (200, 0), (300, 1)]);
        assert!(rows[0].has_children && !rows[2].has_children);
    }

    #[test]
    fn system_adopts_children_reported_as_older_than_itself() {
        // Windows 11 reports Secure System and Registry as created seconds before System itself.
        const BOOT: i64 = 1_000_000_000;
        let p = provider(&[
            (100, 4, BOOT - 85_000_000, "Secure System"),
            (104, 4, BOOT - 84_000_000, "Registry"),
            (0, 0, BOOT, ""),
            (4, 0, BOOT, "System"),
            (300, 4, BOOT + 200_000, "smss.exe"),
        ]);
        let none = HashSet::new();
        let rows = real(build_rows(p.items(), &options(&none)));
        assert_eq!(layout(&rows), vec![(0, 0), (4, 1), (100, 2), (104, 2), (300, 2)]);
    }

    #[test]
    fn recycled_parent_pid_does_not_adopt_older_processes() {
        // PID 50 now belongs to a process created *after* 60, so it can't be 60's parent.
        let p = provider(&[(50, 1, 100, "new-owner.exe"), (60, 50, 40, "orphan.exe")]);
        let none = HashSet::new();
        let rows = real(build_rows(p.items(), &options(&none)));
        assert_eq!(layout(&rows), vec![(60, 0), (50, 0)]);
    }

    #[test]
    fn collapsed_nodes_hide_their_descendants() {
        let p = provider(&[(4, 0, 0, "System"), (100, 4, 10, "a"), (101, 100, 11, "b"), (5, 0, 1, "c")]);
        let collapsed = HashSet::from([ProcessKey { pid: 100, create_time: 10 }]);
        let rows = real(build_rows(p.items(), &options(&collapsed)));
        assert_eq!(layout(&rows), vec![(4, 0), (100, 1), (5, 0)]);
        assert!(rows[1].has_children && !rows[1].expanded);
    }

    #[test]
    fn filter_matches_name_or_pid_and_flattens() {
        let p = provider(&[
            (4, 0, 0, "System"),
            (1234, 4, 10, "Chrome.exe"),
            (777, 1234, 11, "chrome.exe"),
            (88, 4, 12, "x"),
        ]);
        let none = HashSet::new();
        let rows = real(build_rows(p.items(), &ViewOptions { filter: "CHROME", ..options(&none) }));
        assert_eq!(layout(&rows), vec![(1234, 0), (777, 0)]);
        let rows = real(build_rows(p.items(), &ViewOptions { filter: "12", ..options(&none) }));
        assert_eq!(layout(&rows), vec![(1234, 0)]);
    }

    #[test]
    fn sorts_siblings_within_the_tree() {
        let p = provider(&[(4, 0, 0, "System"), (10, 4, 1, "zeta"), (11, 4, 2, "Alpha"), (12, 4, 3, "mid")]);
        let none = HashSet::new();
        let rows = real(build_rows(p.items(), &ViewOptions { sort: SortColumn::Name, ..options(&none) }));
        assert_eq!(layout(&rows), vec![(4, 0), (11, 1), (12, 1), (10, 1)]);
        let rows = real(build_rows(
            p.items(),
            &ViewOptions { sort: SortColumn::Name, order: SortOrder::Descending, tree: false, ..options(&none) },
        ));
        assert_eq!(rows.iter().map(|r| r.key.pid).collect::<Vec<_>>(), vec![10, 4, 12, 11]);
    }
}
