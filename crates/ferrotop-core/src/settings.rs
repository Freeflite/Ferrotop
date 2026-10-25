//! User preferences, saved to `%APPDATA%\Ferrotop\settings.ini` as `key=value` lines.
//!
//! Loading never fails: unknown keys, bad values and a missing file all fall back to defaults.

use std::fmt::Write;
use std::path::PathBuf;

use crate::tree::{SortColumn, SortOrder};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ThemeMode {
    #[default]
    Light,
    Dark,
    /// Follow Windows' app light/dark setting.
    System,
}

impl ThemeMode {
    pub const ALL: [ThemeMode; 3] = [ThemeMode::Light, ThemeMode::Dark, ThemeMode::System];

    pub fn id(self) -> &'static str {
        match self {
            ThemeMode::Light => "light",
            ThemeMode::Dark => "dark",
            ThemeMode::System => "system",
        }
    }
}

/// How the Performance page graphs the CPU.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CpuGraph {
    #[default]
    Overall,
    /// One graph per physical core.
    Cores,
    /// One graph per logical processor (hardware thread).
    Logical,
    /// One graph per group of cores sharing an L3 cache (a CCX on AMD).
    Ccx,
}

impl CpuGraph {
    pub const ALL: [CpuGraph; 4] = [CpuGraph::Overall, CpuGraph::Cores, CpuGraph::Logical, CpuGraph::Ccx];

    pub fn id(self) -> &'static str {
        match self {
            CpuGraph::Overall => "overall",
            CpuGraph::Cores => "cores",
            CpuGraph::Logical => "logical",
            CpuGraph::Ccx => "ccx",
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Page {
    #[default]
    Processes,
    Performance,
    Services,
    Network,
    History,
    Windows,
}

impl Page {
    /// In tab order.
    pub const ALL: [Page; 6] =
        [Page::Processes, Page::Performance, Page::Services, Page::Network, Page::History, Page::Windows];

    pub fn id(self) -> &'static str {
        match self {
            Page::Processes => "processes",
            Page::Performance => "performance",
            Page::Services => "services",
            Page::Network => "network",
            Page::History => "history",
            Page::Windows => "windows",
        }
    }

    /// The UI's number for the page (pages in between are sub-pages like Find).
    pub fn index(self) -> i32 {
        match self {
            Page::Processes => 0,
            Page::Performance => 1,
            Page::Services => 2,
            Page::Network => 3,
            Page::History => 7,
            Page::Windows => 8,
        }
    }

    pub fn from_index(index: i32) -> Option<Page> {
        Page::ALL.into_iter().find(|page| page.index() == index)
    }
}

/// A graph id is short lower-case words joined by dots, e.g. `gpu.temperature`.
fn is_graph_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 40 && id.bytes().all(|b| b.is_ascii_lowercase() || b == b'.' || b == b'-')
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub theme: ThemeMode,
    /// The page and Performance device that were showing last time, restored on startup.
    pub page: Page,
    /// A device id such as `cpu`, `disk:1` or `gpu:0`.
    pub device: String,
    pub cpu_graph: CpuGraph,
    pub tree: bool,
    pub sort: SortColumn,
    pub order: SortOrder,
    /// Visible columns besides Name (which is always shown).
    pub columns: Vec<SortColumn>,
    /// Logical window size.
    pub window: Option<(u32, u32)>,
    /// Performance graphs the user turned off, by id (see `performance::GraphId`).
    pub hidden_graphs: Vec<String>,
    /// Where LibreHardwareMonitor's web server listens, for CPU temperatures without PawnIO.
    pub lhm_port: u16,
    /// Closing the window leaves Ferrotop running in the tray.
    pub close_to_tray: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: ThemeMode::Light,
            page: Page::Processes,
            device: "cpu".into(),
            cpu_graph: CpuGraph::Overall,
            tree: true,
            sort: SortColumn::Natural,
            order: SortOrder::Ascending,
            // System Informer's default columns.
            columns: vec![
                SortColumn::Pid,
                SortColumn::Cpu,
                SortColumn::IoRate,
                SortColumn::PrivateBytes,
                SortColumn::WorkingSet,
                SortColumn::User,
                SortColumn::Description,
                SortColumn::Gpu,
                SortColumn::GpuDedicated,
            ],
            window: None,
            hidden_graphs: Vec::new(),
            lhm_port: crate::lhm::DEFAULT_PORT,
            close_to_tray: true,
        }
    }
}

impl Settings {
    pub fn parse(text: &str) -> Self {
        let mut settings = Settings::default();
        let mut version = 1;
        let mut has_columns = false;
        for (key, value) in text.lines().filter_map(|line| line.split_once('=')) {
            let value = value.trim();
            match key.trim() {
                "version" => version = value.parse().unwrap_or(1),
                "theme" => {
                    if let Some(theme) = ThemeMode::ALL.into_iter().find(|t| t.id() == value) {
                        settings.theme = theme;
                    }
                }
                "tree" => settings.tree = value != "false",
                "page" => settings.page = Page::ALL.into_iter().find(|p| p.id() == value).unwrap_or_default(),
                "device" if !value.is_empty() && value.len() < 64 => settings.device = value.to_owned(),
                "cpu-graph" => {
                    if let Some(graph) = CpuGraph::ALL.into_iter().find(|g| g.id() == value) {
                        settings.cpu_graph = graph;
                    }
                }
                "sort" => settings.sort = SortColumn::from_id(value).unwrap_or(settings.sort),
                "order" => settings.order = if value == "desc" { SortOrder::Descending } else { SortOrder::Ascending },
                "columns" => {
                    has_columns = true;
                    settings.columns = value
                        .split(',')
                        .filter_map(|id| SortColumn::from_id(id.trim()))
                        .filter(|c| !matches!(c, SortColumn::Name | SortColumn::Natural))
                        .collect();
                }
                "hidden-graphs" => {
                    settings.hidden_graphs = value
                        .split(',')
                        .map(str::trim)
                        .filter(|id| is_graph_id(id))
                        .take(64)
                        .map(str::to_owned)
                        .collect();
                }
                "close-to-tray" => settings.close_to_tray = value != "false",
                "lhm-port" => settings.lhm_port = value.parse().ok().filter(|p| *p != 0).unwrap_or(settings.lhm_port),
                "window" => {
                    settings.window = value
                        .split_once('x')
                        .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
                        .filter(|&(w, h): &(u32, u32)| (400..=10_000).contains(&w) && (300..=10_000).contains(&h));
                }
                _ => {}
            }
        }
        // Version 2 added the GPU columns; show them for people upgrading from version 1.
        if version < 2 && has_columns {
            for column in [SortColumn::Gpu, SortColumn::GpuDedicated] {
                if !settings.columns.contains(&column) {
                    settings.columns.push(column);
                }
            }
        }
        settings
    }

    pub fn serialize(&self) -> String {
        let mut text = String::from("# Ferrotop settings\nversion=3\n");
        let _ = writeln!(text, "theme={}", self.theme.id());
        let _ = writeln!(text, "tree={}", self.tree);
        let _ = writeln!(text, "page={}", self.page.id());
        let _ = writeln!(text, "device={}", self.device);
        let _ = writeln!(text, "cpu-graph={}", self.cpu_graph.id());
        let _ = writeln!(text, "sort={}", self.sort.id());
        let _ = writeln!(text, "order={}", if self.order == SortOrder::Descending { "desc" } else { "asc" });
        let columns: Vec<&str> = self.columns.iter().map(|c| c.id()).collect();
        let _ = writeln!(text, "columns={}", columns.join(","));
        if let Some((width, height)) = self.window {
            let _ = writeln!(text, "window={width}x{height}");
        }
        let _ = writeln!(text, "hidden-graphs={}", self.hidden_graphs.join(","));
        let _ = writeln!(text, "close-to-tray={}", self.close_to_tray);
        let _ = writeln!(text, "lhm-port={}", self.lhm_port);
        text
    }

    fn path() -> Option<PathBuf> {
        Some(PathBuf::from(std::env::var_os("APPDATA")?).join("Ferrotop").join("settings.ini"))
    }

    pub fn load() -> Self {
        Self::path().and_then(|path| std::fs::read_to_string(path).ok()).map_or_else(Self::default, |t| Self::parse(&t))
    }

    /// Writes the settings, replacing the file atomically so a crash can't leave it half-written.
    pub fn save(&self) -> std::io::Result<()> {
        let path = Self::path().ok_or_else(|| std::io::Error::other("APPDATA is not set"))?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let temp = path.with_extension("ini.tmp");
        std::fs::write(&temp, self.serialize())?;
        std::fs::rename(&temp, &path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let settings = Settings {
            theme: ThemeMode::Dark,
            cpu_graph: CpuGraph::Logical,
            page: Page::Performance,
            device: "gpu:0".into(),
            tree: false,
            sort: SortColumn::Cpu,
            order: SortOrder::Descending,
            columns: vec![SortColumn::Cpu, SortColumn::Handles],
            window: Some((1200, 800)),
            hidden_graphs: vec!["gpu.temperature".into(), "disk.transfer".into()],
            lhm_port: 9000,
            close_to_tray: false,
        };
        assert_eq!(Settings::parse(&settings.serialize()), settings);
    }

    #[test]
    fn version_one_settings_gain_the_gpu_columns() {
        let upgraded = Settings::parse("theme=light\ncolumns=pid,cpu");
        assert_eq!(upgraded.columns, [SortColumn::Pid, SortColumn::Cpu, SortColumn::Gpu, SortColumn::GpuDedicated]);
        let current = Settings::parse("version=2\ncolumns=pid,cpu");
        assert_eq!(current.columns, [SortColumn::Pid, SortColumn::Cpu], "a v2 choice to hide them sticks");
    }

    #[test]
    fn junk_falls_back_to_defaults() {
        let parsed = Settings::parse("version=2\ntheme=neon\nsort=bogus\nwindow=5x5\ncolumns=name,cpu,,nope\ngarbage");
        assert_eq!(parsed.theme, ThemeMode::Light);
        assert_eq!(parsed.sort, SortColumn::Natural);
        assert_eq!(parsed.window, None);
        assert_eq!(parsed.columns, vec![SortColumn::Cpu]);
        assert_eq!(Settings::parse(""), Settings::default());
        let parsed = Settings::parse("page=services\nhidden-graphs=cpu.temperature,BAD,../x,\nlhm-port=0");
        assert_eq!(parsed.page, Page::Services);
        assert_eq!(parsed.hidden_graphs, ["cpu.temperature"]);
        assert_eq!(parsed.lhm_port, crate::lhm::DEFAULT_PORT);
    }
}
