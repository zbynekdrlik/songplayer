# Spout2 SDK 2.007.017 — the SpoutDX sender sources (vendored)

The `SP-program-MAX` Spout sender (#223 S1b) is built from these files by
`crates/sp-gpu/build.rs` (the `cc` crate, MSVC, Windows targets only) together
with the shim `crates/sp-gpu/src/win/spout_shim.cpp`.

- **Upstream:** Spout2 by Lynn Jarvis, https://github.com/leadedge/Spout2
- **Version:** 2.007.017 (git tag `2.007.017`, release
  https://github.com/leadedge/Spout2/releases/tag/2.007.017)
- **Licence:** BSD 2-Clause, `LICENSE` here (the repository root's `LICENSE`
  of that tag). Every source file carries the same notice in its header.
- **Unmodified:** every file is byte for byte the tag's file. Nothing here is
  edited; a change of behaviour goes in the shim.

## Files

Exactly the sources of upstream's `SpoutDX_static` library (CMake
`SPOUTSDK/SpoutDirectX/SpoutDX/CMakeLists.txt`, `SpoutDX_SOURCES` and
`SpoutDX_HEADERS`), copied flat into this one folder. `SpoutDX.h` then takes
its same-folder include branch (`__has_include("SpoutCommon.h")`).

| File | Upstream path |
|---|---|
| `SpoutDX.cpp`, `SpoutDX.h` | `SPOUTSDK/SpoutDirectX/SpoutDX/` |
| `SpoutCommon.h` | `SPOUTSDK/SpoutGL/` |
| `SpoutCopy.cpp`, `SpoutCopy.h` | `SPOUTSDK/SpoutGL/` |
| `SpoutDirectX.cpp`, `SpoutDirectX.h` | `SPOUTSDK/SpoutGL/` |
| `SpoutFrameCount.cpp`, `SpoutFrameCount.h` | `SPOUTSDK/SpoutGL/` |
| `SpoutSenderNames.cpp`, `SpoutSenderNames.h` | `SPOUTSDK/SpoutGL/` |
| `SpoutSharedMemory.cpp`, `SpoutSharedMemory.h` | `SPOUTSDK/SpoutGL/` |
| `SpoutUtils.cpp`, `SpoutUtils.h` | `SPOUTSDK/SpoutGL/` |
| `LICENSE` | `LICENSE` (repository root) |

`spoutDX` (the sender) keeps a `spoutCopy` member and its `.cpp` also holds
the receiver functions, so all seven `.cpp` files link even though only the
sender path runs.

## Build flags (from upstream's own builds)

- `SPOUT_BUILD_STATIC` (CMake's static library), `NDEBUG` (CMake Release).
- Multibyte: no `UNICODE` / `_UNICODE` (the vcxproj's `CharacterSet`).
- `/std:c++17` (the x64 Release vcxproj's `LanguageStandard`).
- `/EHsc`: the SDK uses try/catch, and `cc` adds no exception model for MSVC.
- Compiler warnings off for these files (they are not ours to fix).

## Bumping the SDK

1. Download the new tag's source
   (`https://github.com/leadedge/Spout2/archive/refs/tags/<tag>.tar.gz`) and
   read its `SpoutDX_SOURCES` / `SpoutDX_HEADERS`: if the list changed, change
   this folder and `build.rs` to match.
2. Copy the files over these, unmodified, and the root `LICENSE`.
3. `diff` everything the shim (`src/win/spout_shim.cpp`) calls against the
   facts in `.claude/rules/gpu-max.md`: `spoutDX::SendTexture`,
   `CheckSender`, `SetSenderName`, `GetName`, `IsInitialized`,
   `ReleaseSender`, `GetWidth` / `GetHeight`, `OpenDirectX11` /
   `CloseDirectX11`, the public member `spoutDX::sendernames`, and
   `spoutSenderNames::CreateSender` / `RegisterSenderName` /
   `ReleaseSenderName` / `CleanSenders` / `GetSenderNames` /
   `FindSenderName` / `FindSender(const char*)` (upstream marks the last
   "Used for testing - may be removed"; its this-object-only meaning is what
   keeps a refusal from releasing another sender's name). Also the
   `SharedTextureInfo` layout and the `SpoutSenderNames` map format
   (`src/spout.rs` parses them).
4. Update the version here and in `.claude/rules/gpu-max.md`; the WARP tests
   in `tests/spout.rs` are the gate (CI, `Build (Windows)`).
