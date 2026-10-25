# Builds target\release\ferrotop.exe for distribution.
#
# Rust bakes source paths into binaries (for panic messages), which would publish the build
# machine's user name and folder layout. This remaps them to neutral prefixes, then refuses to
# finish if the user name, machine name or repo path still appears anywhere in the exe, or if the
# exe lacks the expected exploit mitigations (see check-pe.ps1).
#
# -Verify builds a second time in another target directory and checks both builds are
# byte-for-byte identical (a reproducible build).
#
# Usage: pwsh scripts\build-release.ps1 [-Verify]
param([switch]$Verify)
$ErrorActionPreference = 'Stop'

$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
$rustupHome = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $env:USERPROFILE '.rustup' }

function Build([string]$targetDir) {
    # RUSTFLAGS replaces .cargo/config.toml's rustflags, so the hardening flags are repeated here.
    # When several prefixes match, rustc applies the last one, so the target directory (inside
    # the repo) comes after the repo itself.
    $flags = @(
        '-C', 'target-feature=+crt-static',
        '-C', 'control-flow-guard',
        '-C', 'link-arg=/DEPENDENTLOADFLAG:0x800',
        '-C', 'link-arg=/CETCOMPAT',
        '-C', 'link-arg=/Brepro',
        "--remap-path-prefix=$cargoHome=/cargo",
        "--remap-path-prefix=$rustupHome=/rustup",
        "--remap-path-prefix=$repo=/ferrotop",
        "--remap-path-prefix=$targetDir=/target"
    )
    $env:CARGO_ENCODED_RUSTFLAGS = $flags -join [char]0x1f
    try {
        cargo build --release --locked --manifest-path (Join-Path $repo 'Cargo.toml') --target-dir $targetDir
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed ($LASTEXITCODE)" }
    } finally {
        Remove-Item Env:\CARGO_ENCODED_RUSTFLAGS
    }
    Join-Path $targetDir 'release\ferrotop.exe'
}

$exe = Build (Join-Path $repo 'target')

$bytes = [IO.File]::ReadAllBytes($exe)
$text = [Text.Encoding]::Latin1.GetString($bytes)
$unicode = [Text.Encoding]::Unicode.GetString($bytes)
$leaks = @($env:USERNAME, $env:COMPUTERNAME, $repo, $env:USERPROFILE) | Where-Object {
    $_ -and ($text.IndexOf($_, [StringComparison]::OrdinalIgnoreCase) -ge 0 -or
             $unicode.IndexOf($_, [StringComparison]::OrdinalIgnoreCase) -ge 0)
}
if ($leaks) { throw "ferrotop.exe still contains build-machine details: $($leaks -join ', ')" }

"Exploit mitigations:"
& (Join-Path $PSScriptRoot 'check-pe.ps1') $exe

$hash = (Get-FileHash $exe -Algorithm SHA256).Hash
if ($Verify) {
    $again = Build (Join-Path $repo 'target\repro')
    $second = (Get-FileHash $again -Algorithm SHA256).Hash
    if ($second -ne $hash) { throw "Builds differ: $hash vs $second" }
    "Reproducible: a second build in another directory is byte-for-byte identical."
}
"Built $exe ($([math]::Round($bytes.Length / 1MB, 1)) MB), no build-machine paths. SHA-256 $hash"
