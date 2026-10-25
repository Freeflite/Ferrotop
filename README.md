# Ferrotop

A fast, memory-safe process explorer for Windows, written in Rust with a [Slint](https://slint.dev) UI.

Ferrotop is a Rust re-imagining of [System Informer](https://github.com/winsiderss/systeminformer)
(formerly Process Hacker). Its process provider, CPU accounting, process-tree rules and NT
structure layouts are ported from System Informer's C sources (MIT). See
[THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md).

> **Status: v0.6 in progress.** Processes, process properties, performance, history,
> services, network connections and the common process actions are in. See
> [docs/ROADMAP.md](docs/ROADMAP.md) for what's next.

## What it does

### Processes

- Live process **tree** (or flat list), refreshed every second, with each program's **icon**.
  Parent/child links are safe against PID reuse: a recycled PID never adopts another process's
  children.
- **Cycle-based CPU usage**, System Informer's default and more accurate than time sampling,
  including the final cycles of processes that exit between samples, with an **Interrupts** row
  for CPU spent in DPCs and interrupts.
- Columns for **CPU**, **GPU**, **RAM** (private working set, Task Manager's "Memory"), **I/O
  rate**, **disk** and **network** activity, **private bytes**, **working set**, **GPU memory**
  (dedicated and shared), **threads**, **handles**, **user name**, **description** and
  **verified signer** (Authenticode, embedded or catalog-signed). Pick them from the View menu;
  drag a heading to move a column, or its right edge to resize it (in every table). Disk and
  network come from a kernel event trace (as in Resource Monitor), so they need Ferrotop to run
  as administrator.
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
    name their target), **Windows** (title, class, state and position; **Bring to front**),
    **Environment**, **Token** (user, integrity, elevation, groups, privileges) and **Services**.
- **Windows** (Ctrl+6): every window on the desktop with the process it belongs to; go to its
  process or bring it to the front.
- **Find handles, DLLs or windows** across every process (View → Find…), or **Pick a window**:
  click any window on screen to jump to the process that owns it. **Run** a new program
  (View → Run…).

### Performance

A page like Task Manager's: a device list with live mini-graphs, and for the selected device:

- **CPU**: utilization overall, per core, per logical processor, or per CCX (cores sharing an L3
  cache), with each CCD's CCXs boxed together: always on multi-CCD Zen to Zen 2 chips, otherwise
  (and for the CCD temperatures) when the temperature sensors can be read. Speed, up time, cores,
  threads and cache sizes, and **CPU temperatures** (see below).
- **Memory**: usage and commit, cached and pool sizes, module speed, type, slots and form factor
  (from the SMBIOS firmware table).
- **Each disk**: active time, throughput, response time, model, type and size.
- **Each network adapter**: send/receive throughput, link speed and IP addresses.
- **Each GPU**: a graph per engine, dedicated and shared memory, temperature, driver and PCI
  location. AMD GPUs add hotspot and memory temperatures, fan, board power and clocks (AMD ADL);
  NVIDIA GPUs add the same through NVML (untested so far).

The View menu lists the selected device's graphs; turn any of them off. With all of a device's
graphs off, its figures fill the pane in large type. **Right-click** any graph, core, device or
figure to show it in History or in the header's small graphs (View → Header graphs… picks
those too).

### History

Ferrotop can record history around the clock, even when its window is closed: every figure the
Performance page shows (each core, CCD and its temperature, each disk and network adapter, GPU
load, engines, memory, temperature, hotspot, power, fan and clocks) every second, and the
busiest programs every minute. A week is kept: every second for the overall figures, every
minute (average and peak) for the rest after the first day. The History page (Ctrl+5) graphs
the last hour, day or week and lists which programs were busiest.

Graphs are yours to arrange: **Add graphs…** picks any recorded figure (or right-click it on the
Performance page), drag a graph by its title to move it, drag its right or bottom edge to resize
it (widths snap to twelfths of the page), and × removes it. **Layouts** offers ready-made
arrangements (Overview, Temperatures, GPU, CPU, Compact) and any layout files in
`%APPDATA%\Ferrotop\Layouts`: **Save this layout to a file** writes the current one there as
JSON to edit, keep or share. **Programs** shows or hides the table of programs; drag the bar
beside it to share the room.

Recording is done by a small Windows service, installed from the History page with
administrator approval. It's built to be safe to leave running: it runs from its own copy in
Program Files, keeps only the three privileges it needs, writes only to
`%ProgramData%\Ferrotop` (which only administrators can change), and takes no input from other
programs. It also lets Ferrotop show per-process disk and network activity and CPU temperatures
without running as administrator. **Stop recording** removes the service and keeps what was
recorded. See [SECURITY.md](SECURITY.md) for the details.

### Services and Network

- **Services**: every service and driver with status, startup type, host process, account,
  description and path. Start, stop and restart (asks first; most need administrator).
- **Network**: every TCP and UDP endpoint (IPv4 and IPv6) with its owning process, addresses,
  ports and state.
- **Ping and trace route** (View → Ping…, or a connection's Ping / Trace route actions): ICMP
  echoes through Windows' own API, no administrator needed. Only the address you give is
  contacted; there's no whois or geolocation lookup, which would send it to a third party.

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
- **Tray icon**: closing the window leaves Ferrotop in the notification area (View → Close to
  tray), with CPU and memory in its tooltip; click it to bring the window back, right-click for
  Exit. View → Start at sign-in starts it there when you sign in. Launching Ferrotop again shows
  the copy that's already running.
- **Run as administrator** from the status bar. When elevated, Ferrotop enables
  `SeDebugPrivilege` (as System Informer does) to read processes owned by services and other
  users.
- Settings (theme, columns and their widths, sort, tree view, page, hidden graphs, History and
  header graphs, window size) are saved to `%APPDATA%\Ferrotop\settings.ini`; History layouts
  to `%APPDATA%\Ferrotop\Layouts`; dump files go to `%LOCALAPPDATA%\Ferrotop\Dumps`.
- The OS is sampled on background threads: processes on one, the Performance view's devices on
  another (each disk's details on its own), so a slow device holds up only itself and each shows
  as soon as it can be read. Slow queries (handles, services, stacks, searches) run on worker
  threads too, so the window never waits on them.

### Keyboard

| Key | Action |
| --- | --- |
| Ctrl+1 … Ctrl+6 | Processes, Performance, Services, Network, History, Windows |
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
