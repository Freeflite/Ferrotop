# Ferrotop

A fast, memory-safe process explorer for Windows, written in Rust with a [Slint](https://slint.dev) UI.

Ferrotop is a Rust re-imagining of [System Informer](https://github.com/winsiderss/systeminformer)
(formerly Process Hacker). Its process provider, CPU accounting, process-tree rules and NT
structure layouts are ported from System Informer's C sources (MIT). See
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

> **Status: v0.5.** Processes, process properties, performance, services, network connections
> and the common process actions are in. See [docs/ROADMAP.md](docs/ROADMAP.md) for what's next.

## What it does

### Processes

- Live process **tree** (or flat list), refreshed every second, with each program's **icon**.
  Parent/child links are safe against PID reuse: a recycled PID never adopts another process's
  children.
- **Cycle-based CPU usage**, System Informer's default and more accurate than time sampling,
  including the final cycles of processes that exit between samples, with an **Interrupts** row
  for CPU spent in DPCs and interrupts.
- Columns for **CPU**, **GPU**, **I/O rate**, **private bytes**, **working set**, **GPU memory**
  (dedicated and shared), **threads**, **handles**, **user name**, **description** and
  **verified signer** (Authenticode, embedded or catalog-signed). Pick them from the View menu.
- **Highlighting** like System Informer's: your processes, your elevated processes, service
  hosts, system-account processes, and processes that just started or exited. Suspended
  processes are marked.
- **Right-click** a process for: Properties, Open file location, End process, End process tree,
  Suspend / Resume, Priority, Set affinity, and Create dump file. Anything irreversible or risky
  asks first, critical system processes (whose end would crash Windows) are refused, and every
  action re-checks the process's creation time on the handle it acts through, so it can never
  land on a process that inherited the PID.
- **Process properties** (double-click or Enter), in tabs:
  - **General**: path, command line, parent, signer, user, integrity, priority, affinity, whether
    it's critical, live usage.
  - **Threads**: live CPU per thread, start address (as `module+offset`), priority, state and
    context switches; **Stack** shows a thread's call stack.
  - **Modules**, **Memory** (every region with type, protection and mapped file; writable and
    executable regions are flagged), **Handles** (type, name, access; process and thread handles
    name their target), **Environment**, **Token** (user, integrity, elevation, groups,
    privileges) and **Services**.
- **Find handles or DLLs** across every process (View → Find…), and **Run** a new program
  (View → Run…).

### Performance

A page like Task Manager's: a device list with live mini-graphs, and for the selected device:

- **CPU**: utilization overall, per core, per logical processor, or per CCX (cores sharing an L3
  cache), with each CCD's CCXs boxed together under its temperature. Speed, up time, cores,
  threads and cache sizes, and **CPU temperatures** (see below).
- **Memory**: usage and commit, cached and pool sizes, module speed, type, slots and form factor
  (from the SMBIOS firmware table).
- **Each disk**: active time, throughput, response time, model, type and size.
- **Each network adapter**: send/receive throughput, link speed and IP addresses.
- **Each GPU**: a graph per engine, dedicated and shared memory, temperature, driver and PCI
  location. AMD GPUs add hotspot and memory temperatures, fan, board power and clocks (AMD ADL);
  NVIDIA GPUs add the same through NVML (untested so far).

The View menu lists the selected device's graphs; turn any of them off. With all of a device's
graphs off, its figures fill the pane in large type.

### Services and Network

- **Services**: every service and driver with status, startup type, host process, account,
  description and path. Start, stop and restart (asks first; most need administrator).
- **Network**: every TCP and UDP endpoint (IPv4 and IPv6) with its owning process, addresses,
  ports and state.

Every table sorts by any column and has its own filter. **Go to process** jumps from a service,
connection or search result to the process.

### CPU temperatures

Neither AMD nor Intel exposes CPU temperatures to normal programs; reading them needs a kernel
driver. Ferrotop ships no driver, but uses one of these if it's there:

1. **[PawnIO](https://pawnio.eu)**, a signed, open-source driver also used by
   LibreHardwareMonitor and FanControl. Install it yourself, run Ferrotop as administrator, and
   you get Tctl (or the Intel package temperature) plus each CCD (or core). Ferrotop talks to the
   driver directly (no PawnIO DLL is loaded) using two official signed modules, whose compiled
   code was checked byte-for-byte against their published source; see
   [`crates/ferrotop-sys/modules/pawnio`](crates/ferrotop-sys/modules/pawnio/README.md).
2. **LibreHardwareMonitor**, if it's running with its web server on (Options → Remote Web
   Server → Run). Ferrotop only ever connects to `127.0.0.1`. The port is `lhm-port` in the
   settings file (default 8085).

### Everything else

- **Light, dark, or follow-Windows theme** (follows live changes). On Windows 11 the native title
  bar is painted in the theme's colors.
- **Run as administrator** from the status bar. When elevated, Ferrotop enables
  `SeDebugPrivilege` (as System Informer does) to read processes owned by services and other
  users.
- Settings (theme, columns, sort, tree view, page, hidden graphs, window size) are saved to
  `%APPDATA%\Ferrotop\settings.ini`; dump files go to `%LOCALAPPDATA%\Ferrotop\Dumps`.
- The OS is sampled on a background thread, and slow queries (handles, services, stacks,
  searches) run on worker threads, so the window never waits on them.

### Keyboard

| Key | Action |
| --- | --- |
| Ctrl+1 … Ctrl+4 | Processes, Performance, Services, Network |
| ↑ / ↓, PgUp / PgDn | Move the selection |
| ← / → | Collapse / expand the selected node |
| Enter, double-click | Process properties |
| Delete | End the selected process (asks first) |
| Ctrl+Tab / Ctrl+Shift+Tab | Next / previous properties tab |
| Esc | Leave the properties or find page; cancel a dialog |
| ↑ / ↓ (Performance) | Previous / next device |

## Safety model

"No vulnerabilities" isn't something any project can promise, but Ferrotop keeps its unsafe
surface small, contained and auditable. More in [SECURITY.md](SECURITY.md).

- **`unsafe` lives in one crate.** `ferrotop-sys` is the only crate that touches FFI.
  `ferrotop-core` is `#![forbid(unsafe_code)]`; the app crate is `#![deny(unsafe_code)]` (only
  Slint's generated glue opts out).
- **One operation per unsafe block, each with a `SAFETY:` comment**, enforced by clippy.
- **Untrusted buffers are never trusted.** Everything the kernel, firmware, another process or
  LibreHardwareMonitor hands back is bounds-checked; self-referencing strings must point inside
  the buffer they came in. The parsers are mutation-tested on every test run, fuzzed with
  cargo-fuzz under AddressSanitizer, and the unsafe helpers run under Miri in CI.
- **Struct layouts are pinned at compile time** against the kernel ABI.
- **Handles are RAII** (`OwnedHandle`), so they close exactly once.
- **DLLs come from System32 only**: for the exe's imports (`/DEPENDENTLOADFLAG`), for run-time
  loads (`SetDefaultDllDirectories`), and explicitly for dbghelp and the GPU vendor libraries.
  Remote and low-integrity images are refused; AppInit DLLs and global hooks are disabled.
- **Exploit mitigations**: Control Flow Guard, CET shadow-stack compatibility, ASLR (high
  entropy), DEP and the segment heap. The release script checks the exe for them.
- **Software rendering only**: no OpenGL or GPU driver in the process.
- **No network**, except the optional loopback request to LibreHardwareMonitor. No telemetry and
  no symbol downloads.

## Download

Get `ferrotop.exe` from [Releases](https://github.com/Freeflite/Ferrotop/releases): one file,
nothing to install, 64-bit Windows 10 or 11. It isn't code-signed, so the first run shows
SmartScreen's "Windows protected your PC"; choose **More info → Run anyway**. Each release lists
SHA-256 checksums and has a signed build-provenance attestation
(`gh attestation verify ferrotop.exe --repo Freeflite/Ferrotop`), or build it yourself below.

## Building

Requires Windows 10/11, the Visual Studio C++ build tools, and Rust. `rust-toolchain.toml` pins
the exact compiler (rustup installs it automatically).

```bash
cargo build --release
```

For a build you'll share, use the release script. It strips the build machine's paths (and so
its user name) from the binary, refuses to finish if any remain, checks the exploit mitigations,
and with `-Verify` rebuilds to prove the build is reproducible:

```bash
pwsh scripts/build-release.ps1 -Verify
```

The result is a single file, `target/release/ferrotop.exe`: the C runtime is linked statically,
so it needs nothing but Windows itself. Releases built by CI carry a signed build-provenance
attestation (`gh attestation verify ferrotop.exe --repo Freeflite/Ferrotop`).

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check                                   # advisories, licenses, sources
cargo +nightly miri test -p ferrotop-sys --lib -- buffer::
cargo +nightly fuzz run sys_buffers                # needs the MSVC ASan DLL on PATH
```

## Layout

| Crate | What it is | Unsafe? |
| --- | --- | --- |
| `crates/ferrotop-sys` | NT/Win32 FFI: processes and threads, modules, memory regions, handles, environment, tokens, services, connections, process control and dumps, stack walking, signatures and icons, performance counters, GPUs, disks, network adapters, CPU topology, SMBIOS, PawnIO | Yes, audited |
| `crates/ferrotop-core` | Sampler, process provider, details and file caches, property tabs, tables, Performance collector and histories, CPU temperature sources, tree builder, settings, formatting | Forbidden |
| `crates/ferrotop` | Slint UI (`ui/*.slint`) and the app wiring | Denied |
| `fuzz` | cargo-fuzz targets for the parsers | |

## License

MIT. See [LICENSE](LICENSE). Third-party components: [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).
