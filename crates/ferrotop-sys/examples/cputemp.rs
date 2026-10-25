//! Prints CPU temperatures read through PawnIO (needs PawnIO installed and an elevated shell).
//! An optional argument writes the report to that file instead of stdout.

use std::io::Write;

fn main() {
    let info = ferrotop_sys::hardware::cpu_info();
    let mut report = format!("cpu: {} ({:?})\n", info.name, ferrotop_sys::cpu_temp::cpu_id());
    match ferrotop_sys::cpu_temp::CpuThermal::open(&info.name, &info.core_threads) {
        Ok(mut thermal) => {
            for _ in 0..3 {
                report += &format!("{:?}\n", thermal.read());
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
        }
        Err(reason) => report += &format!("unavailable: {reason:?}\n"),
    }
    match std::env::args().nth(1) {
        Some(path) => {
            std::fs::File::create(path).and_then(|mut f| f.write_all(report.as_bytes())).expect("write report")
        }
        None => print!("{report}"),
    }
}
