# Packaging and Release

**Status: Normative.** Stage 10.

A download manager that is hard to install does not get used. Packaging is a feature, and it
carries a scorecard weight (row 6).

---

## 1. Targets

| Platform | Format | Priority |
| -------- | ------ | -------: |
| Linux x86_64 | `.deb` (Debian/Ubuntu/Kali) | 1 |
| Linux x86_64 | AppImage (portable, no root) | 1 |
| Windows x86_64 | MSI or NSIS installer | 1 |
| Linux x86_64 | `.rpm` (Fedora/openSUSE) | 2 |
| Linux x86_64 | Flatpak (Flathub) | 2 |
| Linux aarch64 | `.deb` + AppImage | 3 |
| Windows aarch64 | Installer | 3 |
| Linux | AUR `PKGBUILD` (community-maintained) | 3 |
| macOS | — | Out of scope for 1.0 |

`.deb` and AppImage first because they cover the largest share of the Linux desktop with the
least packaging-specific work, and AppImage needs no root — which matters for the users who
most want this tool.

## 2. What ships

| Component | Path (Linux) | Notes |
| --------- | ------------ | ----- |
| `downpourd` | `/usr/bin/` | Daemon |
| `dp` | `/usr/bin/` | CLI |
| `downpour` | `/usr/bin/` | GUI |
| `downpour-host` | `/usr/lib/downpour/` | Native messaging host |
| Native messaging manifests | `/etc/opt/chrome/native-messaging-hosts/`, `/usr/lib/mozilla/native-messaging-hosts/` | System-wide; per-user variants written on first run |
| systemd user unit | `/usr/lib/systemd/user/downpour.service` | `WantedBy=default.target`, not enabled by default |
| Desktop entry, icons | `/usr/share/applications/`, `/usr/share/icons/hicolor/` | |
| Man pages | `/usr/share/man/man1/` | `dp.1`, `downpourd.1` |
| Shell completions | bash, zsh, fish | Generated from `clap` |

Windows: `Program Files\Downpour\`, a Start Menu entry, an optional autostart entry, and
registry keys for native messaging under `HKCU`.

## 3. The daemon's lifecycle on each platform

| Platform | Mechanism | Default |
| -------- | --------- | ------- |
| Linux | systemd **user** service | Installed, **not** enabled. `dp` and the native host start it on demand. |
| Windows | Autostart entry, per-user | Offered during install, defaulting to on-demand |

Never a system-wide service running as root. There is no reason for a download manager to hold
root, and several reasons for it not to.

Socket activation on Linux (`downpour.socket`) is a candidate for post-1.0: zero idle
footprint until the first connection. Not before the lifecycle is otherwise solid.

## 4. Build and release pipeline

GitLab CI (`.gitlab-ci.yml`). The pipeline is guarded on `Cargo.toml` existing, so
the Rust jobs activate in Stage 1 without a pipeline edit.

```
push / PR        → fmt, clippy, unit, property, corpus (fast), deny, audit
nightly          → full simulation, fuzz, cross-platform matrix
tag v*           → build all targets → sign → checksums → draft release
manual approval  → publish release, submit extension updates
```

Cross-compilation: `cargo-dist` for the release matrix, `cargo-xwin` or a Windows runner for
the Windows targets. Linux builds happen in the oldest supported container image so that glibc
symbol requirements stay low; AppImage bundles what it must.

Every release artifact ships with a SHA-256 checksum file and a detached GPG signature over
the checksum file.

## 5. Extension distribution

| Store | Notes |
| ----- | ----- |
| Chrome Web Store | Review turnaround is slow; native messaging and broad host permissions attract scrutiny. Justify every permission in writing. |
| Firefox Add-ons | Source submission required for a minified bundle; keep the build reproducible from the repo. |
| Edge Add-ons | Chromium package, separate submission. |

The extension version and the daemon version must be compatible across a version skew of at
least one minor release in each direction — store review latency guarantees users will run
mismatched pairs. The IPC handshake negotiates; it does not assume.

## 6. Updates

- **Distro packages:** the package manager. Downpour does not self-update when installed from
  a repository.
- **AppImage and Windows installer:** an opt-in check against the GitLab Releases API,
  transmitting nothing but a version string. Signature verified before anything is applied.
- Never a silent update. Never an update that carries an identifier.
- A downgrade attempt must be *refused* by the state layer (I-11), not silently misread.

## 7. Release checklist

Level 3 of `docs/agent/DEFINITION-OF-DONE.md` in operational form.

```
[ ] Scorecard scored with evidence (00-vision-and-scorecard.md)
[ ] Zero silent-corruption findings — release blocker
[ ] Full corpus green; simulation green on the fixed seed set
[ ] Nightly random-seed run green for 3 consecutive nights
[ ] Crash recovery verified: Linux and Windows, SSD and slow-flush device
[ ] Benchmark suite run; results and losses written into the release notes
[ ] Browser matrix verified manually: current Chrome, Edge, Firefox × Linux, Windows
[ ] Clean-machine install verified per target
[ ] Update from the previous release verified; downgrade correctly refused
[ ] cargo deny + cargo audit clean
[ ] Licences and attributions complete for every bundled dependency
[ ] CHANGELOG written for humans, not generated from commit subjects
[ ] Known limitations documented honestly, including what Downpour does not do
[ ] Artifacts signed; checksums published
[ ] state/progress.json reflects the release
```

## 8. Versioning

Semantic versioning.

| Change | Bump |
| ------ | ---- |
| IPC breaking change | Major |
| On-disk format breaking change | Major (with a migration, or a documented refusal) |
| New feature, compatible | Minor |
| Fix, no contract change | Patch |

Pre-1.0 (`0.x`), the minor position carries breaking changes and this is stated in the README.
**1.0 is declared when the scorecard reaches 95 with evidence** — not when the roadmap is
finished. Those are different questions, and conflating them is how projects ship a 1.0 that
does not deserve the number.
