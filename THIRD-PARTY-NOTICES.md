# Third-party notices

## System Informer

Ferrotop ports logic and data-structure layouts from
[System Informer](https://github.com/winsiderss/systeminformer): the process provider and
CPU accounting (`SystemInformer/procprv.c`), the process-tree parent rules
(`SystemInformer/proctree.c`), and NT structure definitions (`phnt/include/ntexapi.h`).
Ferrotop is not affiliated with or endorsed by Winsider Seminars & Solutions.
"System Informer" is their name, and Ferrotop doesn't use it as its own.

```
MIT License

Copyright (c) 2022 Winsider Seminars & Solutions, Inc.

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
```

## PawnIO modules

`crates/ferrotop-sys/modules/pawnio/AMDFamily17.bin` and `IntelMSR.bin` are unmodified, signed
builds of modules from [PawnIO.Modules](https://github.com/namazso/PawnIO.Modules) (release
0.2.11), Copyright (C) namazso, licensed under the GNU Lesser General Public License version 2.1.
Their full source is available from that repository, and the license text is in
`crates/ferrotop-sys/modules/pawnio/COPYING`. Ferrotop loads them, as data, into the separately
installed PawnIO driver; it doesn't link them. The provenance and the byte-for-byte check against
the published source are described in that folder's README. Ferrotop is not affiliated with or
endorsed by the PawnIO project.

## Hardware register facts

The AMD (SMN thermal registers, per-model CCD offsets, Tctl offsets) and Intel (thermal MSR)
register layouts follow the documented behavior of Linux's `k10temp` and `coretemp` drivers. No
code was copied from them.

## App icon

The icon in `crates/ferrotop/assets/` is FreeFlite's own artwork, used with permission of its
owner.

## Slint

The UI is built with [Slint](https://slint.dev), used under the Slint Royalty-free Desktop,
Mobile, and Web Applications License. As that license requires, Ferrotop's About dialog
shows the `AboutSlint` attribution.
