# Security

Ferrotop often runs as Administrator and reads other processes' memory, handles and tokens, so
security problems in it matter. Thank you for taking the time to report one.

## Reporting a vulnerability

Please report privately through GitHub: open the repository's **Security** tab and choose
**Report a vulnerability**. Don't open a public issue for anything exploitable.

Include the Ferrotop version (About box), your Windows version, and steps or a proof of concept.
You'll get an acknowledgement, and a fix or a decision will be published as a GitHub security
advisory. Credit is given unless you'd rather not be named.

## Supported versions

Only the latest release gets security fixes.

## What Ferrotop does and doesn't do

- **No kernel driver of its own.** Everything runs in user mode. CPU temperatures can optionally
  come from the separately installed [PawnIO](https://pawnio.eu) driver, through two official,
  signed modules whose compiled code was checked byte-for-byte against their published source
  (see `crates/ferrotop-sys/modules/pawnio/README.md`). Ferrotop only calls their read functions.
- **No network access**, except an optional request to LibreHardwareMonitor's web server on
  `127.0.0.1` for CPU temperatures, and the ICMP echoes Ping and Trace route send to the address
  you type (a host name is resolved through Windows first). No telemetry, no update checks, no
  whois or geolocation lookups, no symbol downloads (stack walking runs dbghelp in secure mode
  with a local-only search path).
- **Kernel event tracing, when elevated.** As administrator, Ferrotop starts a private ETW
  session (`Ferrotop I/O trace`) that counts disk and network bytes per process. It records only
  event sizes and the issuing thread or process, never contents or addresses, and stops the
  session when it exits (a session left by a crash is stopped on the next start).
- **Pick a window** installs a low-level mouse hook only until your next click, which it
  swallows; it loads nothing into other processes.
- **Reads, doesn't write, other processes,** except for actions you confirm: end, suspend and
  resume, priority, affinity, service start and stop, and dump files. Each action re-checks the
  process's creation time on the handle it acts through, so a recycled PID can't redirect it.
  Critical processes (which would crash Windows) can't be ended or suspended.
- **The optional history service** runs as LocalSystem, so it's designed to take nothing from
  ordinary users:
  - It reads no requests. It only writes: history files to `%ProgramData%\Ferrotop`, and live
    figures to a shared memory section (`Global\Ferrotop.Live`) that users may only read.
    Ferrotop windows treat both as untrusted and check everything they read.
  - It runs from its own copy in `%ProgramFiles%\Ferrotop`, which users can't modify, never from
    wherever you launched Ferrotop.
  - Its folder is created owned by Administrators with a protected ACL (SYSTEM and
    Administrators write, users read). ProgramData lets any user create folders, so an
    existing folder that isn't owned by SYSTEM or Administrators, or a link, is moved aside
    first; the service checks again before it writes anything and won't record into a folder
    that fails.
  - Windows strips every privilege from its token except bypass traverse checking, creating
    global objects (for the live section) and system profiling (for the kernel trace).
  - Installing and removing it are explicit actions that need administrator approval.
- **Files it writes:** `%APPDATA%\Ferrotop\settings.ini`, and dump files you ask for in
  `%LOCALAPPDATA%\Ferrotop\Dumps`. Dumps contain the process's memory, which can include
  passwords and keys, so they stay in your profile.

## Hardening

- All `unsafe` code lives in one crate (`ferrotop-sys`); every block is a single operation with a
  written safety argument, enforced by clippy. The other crates forbid or deny `unsafe`.
- Every buffer the kernel, firmware or another process supplies is bounds-checked before use, and
  the parsers are mutation-tested on every test run (and fuzzed with cargo-fuzz, and run under
  Miri, in CI).
- DLLs load from System32 only, both for the exe's imports (`/DEPENDENTLOADFLAG`) and at run time
  (`SetDefaultDllDirectories`); images from remote shares or low-integrity files are refused, and
  legacy injection points (AppInit DLLs, global hooks) are disabled.
- Release builds use Control Flow Guard, CET shadow-stack compatibility, ASLR and DEP, and the
  release script checks the exe's headers for all of them.
- Dependencies are checked against the RustSec advisory database and a license allowlist
  (`cargo deny`) on every push; only crates.io sources are allowed.
- Release builds are reproducible from a pinned toolchain, contain no build-machine paths, and
  come with a signed GitHub build-provenance attestation. Verify a download with
  `gh attestation verify ferrotop.exe --repo Freeflite/Ferrotop`.
