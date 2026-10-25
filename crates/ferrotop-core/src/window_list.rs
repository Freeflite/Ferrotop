//! The Windows page: every top-level window on the desktop, with the process that owns it.

use std::collections::HashMap;

use ferrotop_sys::desktop::WindowInfo;

use crate::table::{Cell, Column, Table, Tone};

/// Column of the owning process's PID, for "Go to process".
pub const PID_COLUMN: usize = 1;

/// Whether a window is one a person would recognize: shown, and with a title. Most others are
/// invisible helpers or untitled frames (overlays, shadows) that bury the real ones.
pub fn is_notable(window: &WindowInfo) -> bool {
    window.visible && !window.title.trim().is_empty()
}

/// A table of `windows`, named by `names` (PID → process name): the [notable](is_notable) ones,
/// or every one with `include_all`. Each row's ID is the window handle.
pub fn windows_table(windows: &[WindowInfo], names: &HashMap<u32, String>, include_all: bool) -> Table {
    let mut table = Table::new(&[
        Column::text("Process", 180),
        Column::number("PID", 70),
        Column::text("Title", 380),
        Column::text("Class", 240),
        Column::text("State", 90),
        Column::text("Bounds", 200),
        Column::number("Handle", 110),
    ]);
    for window in windows.iter().filter(|w| include_all || is_notable(w)) {
        let bounds = window.rect.map_or_else(String::new, |(left, top, right, bottom)| {
            format!("{} × {} at {left}, {top}", right - left, bottom - top)
        });
        let process = names.get(&window.pid).map_or("", String::as_str);
        table.push(
            window.handle as u64,
            vec![
                Cell::text(process),
                Cell::number(f64::from(window.pid), window.pid.to_string()),
                Cell::text(&window.title),
                Cell::text(&window.class),
                Cell::text(window.state()),
                Cell::text(bounds),
                Cell::hex(window.handle as u64),
            ],
            if window.visible { Tone::Plain } else { Tone::Muted },
        );
    }
    table
}

/// The window handle in a row of [`windows_table`] (its ID).
pub fn handle_of(row: &crate::table::Row) -> usize {
    row.id as usize
}

#[cfg(test)]
mod tests {
    use super::*;

    fn window(pid: u32, handle: usize, visible: bool) -> WindowInfo {
        WindowInfo {
            handle,
            pid,
            tid: 1,
            title: format!("w{handle}"),
            class: "Test".into(),
            visible,
            minimized: false,
            maximized: false,
            rect: Some((0, 0, 100, 50)),
        }
    }

    #[test]
    fn lists_titled_visible_windows_unless_asked_for_all() {
        let untitled = WindowInfo { title: String::new(), ..window(30, 0x400, true) };
        let windows = [window(10, 0x100, true), window(10, 0x200, false), window(20, 0x300, true), untitled];
        let names = HashMap::from([(10, "a.exe".to_owned())]);
        let visible = windows_table(&windows, &names, false);
        assert_eq!(visible.rows.len(), 2);
        assert_eq!(visible.rows[0].cells[0].text, "a.exe");
        assert_eq!(visible.rows[1].cells[0].text, "", "an unknown process stays blank");
        assert_eq!(visible.rows[0].cells[5].text, "100 × 50 at 0, 0");
        assert_eq!(handle_of(&visible.rows[1]), 0x300);
        assert_eq!(windows_table(&windows, &names, true).rows.len(), 4);
    }
}
