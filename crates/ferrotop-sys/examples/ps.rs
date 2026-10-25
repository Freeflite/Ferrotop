//! Minimal process dump: `cargo run -p ferrotop-sys --example ps [name-filter]`.

fn main() -> Result<(), ferrotop_sys::Error> {
    let filter = std::env::args().nth(1).unwrap_or_default().to_ascii_lowercase();
    let mut processes = ferrotop_sys::process::enumerate()?;
    processes.sort_by_key(|p| (p.create_time, p.pid));
    println!("{:>7} {:>7} {:>20}  name", "pid", "ppid", "create_time");
    for p in processes.iter().filter(|p| p.name.to_ascii_lowercase().contains(&filter)) {
        println!("{:>7} {:>7} {:>20}  {}", p.pid, p.parent_pid, p.create_time, p.name);
    }
    Ok(())
}
