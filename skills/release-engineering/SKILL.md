---
name: release-engineering
description: >-
  Use when preparing a release or the machinery around one: choosing a version number, writing a changelog, setting up CI build and test matrices, cross-compiling, and packaging binaries for Windows, macOS and Linux. Publishing itself stays with the user.
---
# Release engineering

A release is reproducible, versioned, tested on what users run, and packaged the way each platform expects. The agent prepares and verifies; pushing tags, publishing packages and uploading artefacts are the user's actions.

## 1. Versioning

Use the scheme the project already uses. For semantic versioning (`MAJOR.MINOR.PATCH`):

| Change | Bump |
|---|---|
| Removes or changes public behaviour in an incompatible way | MAJOR |
| Adds functionality compatibly | MINOR |
| Fixes bugs compatibly | PATCH |
| Before 1.0 | MINOR for incompatible changes, PATCH for the rest |

- Decide "public" explicitly: the API, the command-line interface, configuration keys, file formats, the wire protocol.
- Pre-releases: `1.4.0-rc.1`, `1.4.0-beta.2`. Build metadata after `+` does not affect precedence.
- The version lives in one place (the manifest) and everything else reads it: `--version` output, packages, documentation. Search for stray copies.
- Calendar versioning (`2026.11.0`) suits applications without a compatibility contract.

## 2. Changelog

Write for users, not from commit subjects. Group by kind, newest release first:

```
## 1.4.0 - 2026-11-11

### Added
### Changed
### Deprecated
### Removed
### Fixed
### Security
```

Each entry says what changed for the user and, for breaking changes, how to migrate. Collect candidates with `git log --oneline <last-tag>..HEAD`, then rewrite. Link issues where the project does.

## 3. Release checklist

1. Working tree clean, on the release branch, up to date with the remote.
2. Version bumped in the manifest and lockfile; changelog updated; documentation that names the version updated.
3. Full test suite, linters and formatter pass locally, with a release build.
4. Licence and third-party notices current (`cargo about`, `license-checker`, or the ecosystem's tool).
5. Dependency audit clean or exceptions recorded.
6. Build the artefacts from a clean checkout of the exact commit.
7. Smoke test each artefact on its platform: install, run `--version`, run one real operation.
8. Checksums generated (`sha256sum * > SHA256SUMS`); signatures if the project signs.
9. Hand over to the user: the commit, the tag command (`git tag -a v1.4.0 -m "v1.4.0"`), the artefact paths, and the publish commands. The user tags, pushes and publishes.

## 4. Continuous integration

Structure the pipeline so that fast checks fail first and the matrix covers what you ship.

- **Stages**: format and lint, then build and test, then package. Later stages depend on earlier ones.
- **Matrix**: every operating system and architecture you release for, and the oldest and newest supported toolchain or runtime. Do not multiply dimensions needlessly: test the full set on one platform and a smoke subset on the others when time matters.
- **Pin**: the toolchain version (a toolchain file or setup action input), action versions by full commit hash, and dependency versions through the lockfile (`--locked`, `npm ci`, `--frozen-lockfile`).
- **Cache** dependencies and build output keyed by the lockfile hash and toolchain version.
- **Fail fast off** for matrices, so one platform's failure does not hide another's.
- **Secrets**: never exposed to pull requests from forks; publishing jobs run only on tags or protected branches and request the minimum permissions.
- **Release job**: triggered by a version tag, builds every target, uploads artefacts and checksums, and creates a draft release for a human to publish.

Verify a workflow change by reading the run's logs, not by assuming; ask the user to trigger it when you cannot.

## 5. Cross-compilation

| Ecosystem | How |
|---|---|
| Rust | `rustup target add <triple>`; a linker for the target (`cross` or `cargo-zigbuild` remove most of the pain); `x86_64-unknown-linux-musl` for static Linux binaries; `cargo build --release --target <triple>` |
| Go | `GOOS=<os> GOARCH=<arch> go build`; set `CGO_ENABLED=0` unless C code is required |
| C and C++ | A toolchain file per target for CMake; `zig cc` as a cross compiler; watch the C library version you link against |
| Node, Python | Usually build on each platform in CI; native modules need per-platform wheels or prebuilds |
| .NET | `dotnet publish -r <rid> --self-contained` |

Rules:

- Link against the oldest C library you intend to support on Linux (build in an old distribution image, or use musl), because binaries do not run on older glibc than they were built with.
- macOS: build both `x86_64` and `aarch64`, then merge with `lipo -create` for a universal binary. Set the deployment target (`MACOSX_DEPLOYMENT_TARGET`).
- Windows: choose the MSVC or GNU ABI deliberately; ship or statically link the C runtime.
- Test cross-built artefacts on the real target or an emulator (`qemu-user`, a CI runner of that architecture). A binary that was never run is not released.

## 6. Packaging per platform

| Platform | Formats | Points to get right |
|---|---|---|
| Windows | `.zip` of the executable; MSI (WiX) or MSIX installer; winget and Scoop manifests | Code signing so SmartScreen does not block; version resource in the executable; install path and `PATH` entry; uninstall |
| macOS | `.tar.gz` or `.zip`; `.dmg` or `.pkg` for applications; Homebrew formula or cask | Developer ID signing, hardened runtime, notarisation and stapling; universal binary |
| Linux | `.tar.gz` with a static binary; `.deb` and `.rpm` (`cargo-deb`, `cargo-generate-rpm`, `nfpm`); Flatpak, Snap or AppImage for desktop applications; container image | Dependencies declared; files under `/usr/bin`, `/usr/share`; systemd unit and config under `/etc` when a service; images run as a non-root user and are built from a pinned base |
| Language registries | crates.io, npm, PyPI, NuGet | Package contents checked with a dry run (`cargo package --list`, `npm pack --dry-run`); no secrets or build junk; metadata and licence fields set |

Every archive contains the binary, the licence, and a short README; names include the version and target (`tool-1.4.0-x86_64-unknown-linux-musl.tar.gz`).

## 7. Reproducibility and provenance

- Builds from the same commit give the same output: pinned toolchain, locked dependencies, no timestamps or absolute paths baked in (`SOURCE_DATE_EPOCH`, path remapping).
- Record in the artefact how it was built: version, commit, target (`--version --verbose`).
- Generate a software bill of materials when users or policy ask for one (CycloneDX or SPDX tools for the ecosystem).

## 8. After release

- Verify the published artefact by installing it the way a user would, on a clean machine or container.
- Bump the development version if the project uses one.
- Note anything that went wrong in the release checklist so the next release does not repeat it.

## Report

State the version and why that bump, what the changelog says, which targets were built and smoke-tested (with commands and results), where the artefacts and checksums are, and the exact commands left for the user to tag and publish.
