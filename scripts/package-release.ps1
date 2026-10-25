# Packages a built ferrotop.exe for a GitHub release, into dist\:
#   ferrotop.exe                        the program itself
#   ferrotop-<version>-windows-x64.zip  the program with its licenses (the exe embeds two
#                                       LGPL-2.1 PawnIO modules, whose license must travel with it)
#   SHA256SUMS.txt                      checksums, in `sha256sum -c` format
#
# Usage: pwsh scripts\package-release.ps1 [-Tag v0.5.0] [-Exe <path>] [-OutDir <path>]
# With -Tag, refuses to package unless the tag, Cargo.toml and the exe all agree on the version.
param([string]$Tag, [string]$Exe, [string]$OutDir)
$ErrorActionPreference = 'Stop'

$repo = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
if (-not $Exe) { $Exe = Join-Path $repo 'target\release\ferrotop.exe' }
if (-not $OutDir) { $OutDir = Join-Path $repo 'dist' }

$manifest = Get-Content (Join-Path $repo 'Cargo.toml') -Raw
if ($manifest -notmatch '(?m)^version = "([0-9]+\.[0-9]+\.[0-9]+)"') { throw 'No workspace version in Cargo.toml' }
$version = $Matches[1]
$built = (Get-Item $Exe).VersionInfo.ProductVersion
if ($built -ne $version) { throw "ferrotop.exe reports version $built, Cargo.toml says $version" }
if ($Tag -and $Tag -ne "v$version") { throw "Tag $Tag doesn't match version $version" }

if (Test-Path $OutDir) { Remove-Item $OutDir -Recurse -Force }
$name = "ferrotop-$version-windows-x64"
$staging = Join-Path $OutDir $name
New-Item -ItemType Directory $staging | Out-Null
Copy-Item $Exe (Join-Path $staging 'ferrotop.exe')
Copy-Item (Join-Path $repo 'LICENSE') (Join-Path $staging 'LICENSE.txt')
Copy-Item (Join-Path $repo 'THIRD-PARTY-NOTICES.md') $staging
Copy-Item (Join-Path $repo 'crates\ferrotop-sys\modules\pawnio\COPYING') (Join-Path $staging 'PawnIO-modules-LGPL-2.1.txt')
Compress-Archive -Path (Join-Path $staging '*') -DestinationPath (Join-Path $OutDir "$name.zip")
Remove-Item $staging -Recurse
Copy-Item $Exe (Join-Path $OutDir 'ferrotop.exe')

# LF line endings, so `sha256sum -c SHA256SUMS.txt` works as well as Get-FileHash.
$sums = Get-ChildItem $OutDir -File | Sort-Object Name | ForEach-Object {
    '{0}  {1}' -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLowerInvariant(), $_.Name
}
[IO.File]::WriteAllText((Join-Path $OutDir 'SHA256SUMS.txt'), (($sums -join "`n") + "`n"))

"Packaged Ferrotop $version into ${OutDir}:"
Get-ChildItem $OutDir | ForEach-Object { '  {0,-34} {1,8:N1} MB' -f $_.Name, ($_.Length / 1MB) }
