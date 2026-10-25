//! Prints AMD GPU sensors through ADL: `cargo run -p ferrotop-sys --example amd`.

fn main() {
    let Some(adl) = ferrotop_sys::amd::Adl::open() else {
        println!("ADL not available (no AMD driver)");
        return;
    };
    println!("{adl:?}");
    let adapters = ferrotop_sys::gpu::Adapters::open().expect("adapters");
    for info in adapters.infos() {
        let sensors = info.location.and_then(|location| adl.sensors(location));
        println!("{} at {:?}: {sensors:?}", info.name, info.location);
    }
}
