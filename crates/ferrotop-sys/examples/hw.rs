//! Prints what the Performance view's data sources report: `cargo run -p ferrotop-sys --example hw`.

use ferrotop_sys::{disk, gpu, hardware, net};

fn main() {
    println!("{:#?}", hardware::cpu_info());
    println!("{:#?}", hardware::memory_counters());
    println!("installed: {:?}", hardware::installed_memory());
    for slot in hardware::memory_slots().unwrap_or_default() {
        println!("slot {slot:?}");
    }
    match gpu::Adapters::open() {
        Ok(adapters) => {
            for (index, info) in adapters.infos().enumerate() {
                println!("gpu {index}: {info:?} {:?}", adapters.sensors(index));
            }
        }
        Err(error) => println!("gpu: {error}"),
    }
    for number in 0..8 {
        if let Ok(info) = disk::disk_info(number) {
            println!("disk {number}: {} | {} | {:?}", info.model, info.kind(), info.size);
        }
    }
    for adapter in net::adapters().unwrap_or_default() {
        println!("net: {adapter:?}");
    }
}
