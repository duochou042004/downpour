---
name: package-target
description: Build and verify a Downpour package for one distribution target — deb, AppImage, MSI, rpm or Flatpak — including a clean-machine install check. Use during Stage 10 packaging work.
when_to_use: Building a release artifact; adding a new target; a package installs but does not work; verifying an installer on a clean machine.
argument-hint: "[target: deb | appimage | msi | rpm | flatpak]"
allowed-tools: Read, Write, Edit, Bash, Grep, Glob
---

# Package one target

Read `docs/11-packaging-release.md` first. A download manager that is hard to install does not
get used — packaging carries a scorecard weight.

## What must be in every package

| Component | Notes |
| --------- | ----- |
| `downpourd` | The daemon |
| `dp` | CLI |
| `downpour` | GUI |
| `downpour-host` | Native messaging host, in a private libexec directory |
| Native messaging manifests | Chrome and Firefox, system-wide; per-user written on first run |
| Service definition | systemd **user** unit (Linux) / per-user autostart (Windows). Never system-wide, never root. |
| Desktop entry and icons | Linux |
| Man pages, shell completions | Generated from `clap` |
| Licence and attributions | Every bundled dependency |

## Per-target checklist

- [ ] Builds reproducibly in CI, not only on a developer machine
- [ ] Installs on a **clean** machine (container or VM), not one that already has the toolchain
- [ ] The daemon starts on demand — not enabled by default (`docs/11` §3)
- [ ] The browser finds the native messaging host after install, without a manual step
- [ ] Uninstall removes binaries, manifests and the service, and **leaves user data alone**
- [ ] Upgrade from the previous version preserves the queue and every partial download
- [ ] Downgrade is correctly **refused** by the state layer (I-11), not silently misread
- [ ] SHA-256 checksum file and a detached GPG signature over it
- [ ] Linux: built against the oldest supported glibc so the symbol requirement stays low

## Verification is a fresh machine, always

```bash
podman run --rm -it -v "$PWD/dist:/dist:ro" debian:stable bash
# inside: apt install /dist/downpour_*.deb, then dp daemon start && dp add <url>
```

Testing an installer on the build machine proves nothing — it has every dependency already.
This is the single most common packaging mistake and it is caught only by using a clean image.

## Report

Which target, what was built, what the clean-machine install did, and any manual step a user
would have to perform. A manual step is a defect unless it is documented as intentional.
