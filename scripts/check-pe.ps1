# Checks a 64-bit Windows executable for the hardening Ferrotop's release builds must have, by
# reading its PE headers directly. Exits non-zero (throws) if anything is missing.
#
# Usage: pwsh scripts\check-pe.ps1 target\release\ferrotop.exe
param([Parameter(Mandatory)] [string]$Path)
$ErrorActionPreference = 'Stop'

$bytes = [IO.File]::ReadAllBytes((Resolve-Path $Path))
function U16([int]$at) { [BitConverter]::ToUInt16($bytes, $at) }
function U32([int]$at) { [BitConverter]::ToUInt32($bytes, $at) }

$pe = U32 0x3C
if ((U32 $pe) -ne 0x00004550) { throw "$Path is not a PE file" }
$coff = $pe + 4
$sections = U16 ($coff + 2)
$optionalSize = U16 ($coff + 16)
$optional = $coff + 20
if ((U16 $optional) -ne 0x20B) { throw "$Path is not a 64-bit (PE32+) image" }
$dllCharacteristics = U16 ($optional + 70)

# Load configuration directory (data directory 10) holds DependentLoadFlags and the CFG flags.
$loadConfigRva = U32 ($optional + 112 + 10 * 8)
$sectionTable = $optional + $optionalSize
function FileOffset([uint32]$rva) {
    for ($i = 0; $i -lt $sections; $i++) {
        $s = $sectionTable + $i * 40
        $virtualAddress = U32 ($s + 12); $virtualSize = U32 ($s + 8); $rawPointer = U32 ($s + 20)
        if ($rva -ge $virtualAddress -and $rva -lt $virtualAddress + $virtualSize) { return $rawPointer + ($rva - $virtualAddress) }
    }
    throw "RVA 0x$('{0:X}' -f $rva) is outside every section"
}
if ($loadConfigRva -eq 0) { throw "$Path has no load configuration directory" }
$loadConfig = FileOffset $loadConfigRva
$dependentLoadFlags = U16 ($loadConfig + 78)
$guardFlags = U32 ($loadConfig + 144)

$checks = [ordered]@{
    'ASLR (DYNAMIC_BASE)'                = ($dllCharacteristics -band 0x0040) -ne 0
    'High-entropy ASLR (HIGH_ENTROPY_VA)' = ($dllCharacteristics -band 0x0020) -ne 0
    'DEP (NX_COMPAT)'                     = ($dllCharacteristics -band 0x0100) -ne 0
    'Control Flow Guard (GUARD_CF)'       = ($dllCharacteristics -band 0x4000) -ne 0 -and ($guardFlags -band 0x100) -ne 0
    'Imports from System32 only'          = ($dependentLoadFlags -band 0x0800) -ne 0
}
$failed = $checks.GetEnumerator() | Where-Object { -not $_.Value } | ForEach-Object { $_.Key }
$checks.GetEnumerator() | ForEach-Object { "  {0,-38} {1}" -f $_.Key, $(if ($_.Value) { 'yes' } else { 'NO' }) }
if ($failed) { throw "$Path is missing: $($failed -join ', ')" }
