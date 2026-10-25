# Roadmap

System Informer is about 990k lines of C: a kernel driver (`KSystemInformer`, ~47k), the NT
API headers (`phnt`, ~190k), the core library (`phlib`, ~250k), the application (~210k) and
plugins (~275k). Ferrotop ports it in slices, each one usable on its own. Upstream file names
are noted so each port can be checked against the original.

## v0.1: process monitor (done)

- [x] Process enumeration via `NtQuerySystemInformation(SystemProcessInformation)` with
      bounds-checked parsing (`phnt/ntexapi.h`)
- [x] Process provider: per-process deltas, time-based CPU share, I/O rates, new/exited
      tracking (`procprv.c`)
- [x] Process tree with PID-reuse-safe parent assignment (`proctree.c`)
- [x] Sorting, filtering, expand/collapse, keyboard navigation
- [x] Details pane: image path, command line, parent, session, priority, uptime
- [x] End process with confirmation and a PID-reuse guard
- [x] CPU and memory history graphs, status bar totals
- [x] Unit tests for the provider, tree, formatting and FFI parsing

## v0.2: accuracy and depth (done)

- [x] Sample the OS on a background thread instead of the UI thread
- [x] Cycle-based CPU usage, System Informer's default (`PhpUpdateCpuCycleInformation`,
      `SystemProcessorIdleCycleTimeInformation`), plus the Interrupts pseudo-process
- [x] Count the final cycles of processes that exit mid-interval, through a query handle kept
      for each process
- [x] Enable `SeDebugPrivilege` when elevated; "Run as administrator" relaunch
- [x] User name and token elevation; highlight services, elevated, system, and own processes
- [x] Version info (description, company) and hosted services per process
- [x] Process icons and Authenticode signature verification (embedded and catalog), with a
      Verified signer column
- [x] Column chooser and saved settings
- [x] Light, dark and follow-Windows themes, following live changes
- [x] Single self-contained exe (static C runtime), app icon, version resource, and an
      application manifest (per-monitor DPI v2, long paths, UTF-8, segment heap)
- [x] Theme-matched native title bar (Windows 11 DWM caption/text/border colors)

## v0.3: Performance page and process properties (done)

- [x] Task Manager-style Performance page: CPU, memory, disks, network adapters, GPUs
- [x] CPU graphs overall, per core, per logical processor, and per CCX, grouped by CCD
- [x] GPU engine graphs, GPU memory, temperature, driver and PCI location (D3DKMT + PDH)
- [x] Per-process GPU, dedicated GPU memory and shared GPU memory columns
- [x] Memory module speed/type/slots from SMBIOS; disk model/type/size; adapter IPs
- [x] Show or hide each device's graphs from the View menu; large figures when all are hidden
- [x] Process properties: general, threads with live CPU and start addresses, thread stacks
      (`thrdprv.c`, `thrdstk.c`), modules (`modprv.c`), memory regions (`memprv.c`), handles
      with names (`hndlprv.c`), environment, token, and hosted services

## Temperatures

- [x] Basic GPU temperature for any vendor whose driver reports it (WDDM / D3DKMT)
- [x] AMD GPU edge, hotspot and memory temperatures, fan, board power and clocks (ADL)
- [x] NVIDIA GPU temperature, hottest-point temperature, fan, power and clocks (NVML;
      written to NVIDIA's documented API but untested on real hardware)
- [x] CPU temperatures through the optional, separately installed PawnIO driver: AMD Tctl/Tdie
      and per-CCD (Zen through Zen 5), Intel package and per-core (untested on Intel)
- [x] CPU temperatures from LibreHardwareMonitor's local web server when PawnIO isn't usable
- [ ] Intel Arc extras (hotspot, fan, power) through Intel's Graphics Control Library. Its
      versioned structures are complex enough that it shouldn't ship without a machine to test on.
- [ ] Drive temperatures where the controller passes them through (not RAID or most USB)

## v0.4: system-wide views (done)

- [x] Services and drivers, with start, stop and restart (`srvprv.c`)
- [x] Network connections, TCP and UDP, IPv4 and IPv6 (`netprv.c`)
- [x] System information: covered by the Performance page (`sysinfo.c`)

## v0.5: actions (done)

- [x] Suspend and resume, priority, affinity, end process tree; critical processes refused
- [x] Create dump file (full memory, like Task Manager), find handles or DLLs, run a new task
      (optionally elevated; no run-as-another-user, which would mean handling a password)

## Hardening (ongoing)

- [x] Release builds strip build-machine paths and fail if any remain (`scripts/build-release.ps1`)
- [x] Release builds use CFG, CET compatibility, System32-only imports; the script checks them
- [x] DLL search restricted to System32 at run time; remote/low-integrity images and legacy
      injection points disabled
- [x] CI on `windows-latest`: build, test, clippy, `cargo fmt --check`
- [x] `cargo-deny` for advisories, licenses and sources
- [x] Parsers mutation-tested on every run, fuzzed with cargo-fuzz (ASan), unsafe helpers under Miri
- [x] Reproducible release builds from a pinned toolchain, with a signed build-provenance
      attestation

## Next

- Per-process network and disk activity
- Window list, and searching processes by window
- Plugins' most-used features: network tools, extended GPU and ETW-based disk/network stats
- Running as a service for a persistent history

## Probably out of scope

Authenticode-signed binaries. Ferrotop is published as source: build it with
`scripts/build-release.ps1`, which is reproducible, so anyone can check a build against the code.


A kernel driver like `KSystemInformer`. Rust drivers are possible (`windows-drivers-rs`), but
shipping one means an EV certificate and Microsoft attestation signing, and a driver is
exactly the kind of high-privilege attack surface this project tries to avoid. User mode plus
Administrator and `SeDebugPrivilege` covers most of what people use System Informer for; the
one exception, CPU temperatures, uses a third-party driver only if the user has installed it.
