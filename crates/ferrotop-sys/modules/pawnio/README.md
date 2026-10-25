# PawnIO modules

These two files are the official signed builds of two [PawnIO](https://pawnio.eu) modules. Ferrotop
loads them into the PawnIO driver, if the user has installed PawnIO, to read CPU temperatures.
Ferrotop never installs or ships the driver itself.

| File | Source | SHA-256 |
| --- | --- | --- |
| `AMDFamily17.bin` | `AMDFamily17.p` | `dae74615761b78bdf064dfb3e136252ddcc6fc727d88f14738d0e5800d427a91` |
| `IntelMSR.bin` | `IntelMSR.p` | `d6ed85d65ab17a22f813ef98207d6d537155ee2ded5976a21cb48413c9b92e5f` |

- Release: [namazso/PawnIO.Modules 0.2.11](https://github.com/namazso/PawnIO.Modules/releases/tag/0.2.11)
  (`release_0_2_11.zip`, SHA-256 `43608cb89bc84247fef1368a139013f7d043e17db6d6c8dfc9b46bf0905a81f4`),
  built from commit `52a7e536dff3e53c96917a28caac5e0fa6510696`.
- License: LGPL-2.1 (see `COPYING`). The source is in the repository linked above.
- Ferrotop calls only `ioctl_read_smn` (AMD) and `ioctl_read_msr` (Intel). Both modules check
  every register against a fixed allowlist.

## Verifying them

Each file is a 4-byte little-endian signature length, the author's signature, then the compiled
module. The driver refuses any module whose signature doesn't verify against its built-in key, so
a modified file won't load.

To check that the compiled part matches the published source, build the Pawn compiler 4.1.7152
from CompuPhase's `pawn-4.1.7152.zip` (SHA-256
`c2e7212098a68ad1cbacc8b5cfe44a7e6f95b75a82d1e5d43ccf9330f991a87b`, the same archive the
module repository's `_pawn` source RPM contains). Then compile each module with the flags from
the repository's CI:

```sh
pawncc AMDFamily17.p -iinclude -C64 '-;+' '-(+' -p
```

The resulting `.amx` file is byte-for-byte identical to everything after the signature in the
matching `.bin` file. This was checked for both modules before they were added here.
