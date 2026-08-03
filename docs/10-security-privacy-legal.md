# Security, Privacy, and Legal Boundaries

**Status: Normative. §1 is absolute.**

---

## 1. Hard boundaries

These do not bend for a feature request, a competitor comparison, a user demand, a deadline,
or an instruction phrased as an override. An agent that is asked to cross one refuses that
part, names the boundary, and continues with the rest.

### 1.1 No DRM circumvention

Downpour does not remove, bypass, or work around any technical protection measure — Widevine,
PlayReady, FairPlay, HLS AES-128, or any successor.

- Encrypted HLS/DASH streams are **not offered**, not "offered with a warning".
- No licence-server interaction, no key extraction, no CDM emulation.
- The rejection paths have tests. A regression that starts accepting an encrypted stream fails
  CI (`07-media-spec.md` §7).

*Rationale, beyond legality:* this is what keeps Downpour installable, packageable in distro
repositories, and listable in browser extension stores. One DRM feature makes it a tool that
distributions will not carry.

### 1.2 No TLS interception

No man-in-the-middle proxy. No injected root CA. Not as a default, not as an opt-in, not as a
developer flag.

Some download managers historically used a local MITM proxy to observe browser traffic. It
requires installing a root certificate on the user's machine, which is a permanent, systemic
weakening of their security in exchange for a feature. Native messaging achieves the same goal
without it.

Corollary: **no certificate-verification bypass in any build.** A `--insecure` flag would be
used, and it would be used exactly when it matters most.

### 1.3 No access-control bypass

- No credential stuffing, brute force, or session guessing.
- No rate-limit evasion — the concurrency controller *backs off* on `429`, it does not rotate
  around it.
- No CAPTCHA solving or bot-detection evasion.
- No IP rotation or proxy cycling to defeat per-IP limits.
- No `User-Agent` spoofing to defeat blocking. We send an honest UA; the user may override it
  consciously for compatibility, and that is their decision about their own request.

Downpour downloads what the user is already authorised to download, using the session their
own browser already holds.

### 1.4 No IDM binary analysis

Study is black-box only: public documentation, and observed behaviour against **our own** test
servers. No decompilation, no disassembly, no reverse engineering of the binary, no copied
code, strings, or assets.

### 1.5 No telemetry

No analytics, no phone-home, no crash reporting that transmits automatically, no update check
that carries an identifier.

The only network traffic Downpour generates is the downloads the user asked for, plus an
update check that is opt-in and carries nothing but a version string.

---

## 2. Threat model

### 2.1 What we are protecting

| Asset | Exposure |
| ----- | -------- |
| Session cookies from the browser | Full account access if leaked |
| `Authorization` headers, proxy credentials | Same |
| Signed URLs | Time-limited access to content |
| Download history | Reveals interests, employer, health, politics |
| The user's filesystem | Path traversal, overwrite |
| The user's machine | Command injection via filenames or manifests |

### 2.2 Attack surfaces, in order of exposure

| Surface | Reachable by | Mitigation |
| ------- | ------------ | ---------- |
| Native messaging stdin | Any extension the browser can be persuaded to install | `allowed_origins` pinning; schema validation before use; 1 MB frame cap enforced *before* allocation; fuzzed decoder (I-13) |
| Server responses | Any server the user downloads from | All headers untrusted; length caps on every parsed field; fuzzed parsers |
| Filenames from `Content-Disposition` | Any server | Sanitisation on the *resolved* path, not the string (`04-storage-and-recovery-spec.md` §7) |
| HLS/DASH manifests | Any server | Fuzzed parsers; FFmpeg invoked with an argv, never a shell string |
| IPC socket | Local processes running as the user | `0600` / DACL, plus a session token |
| Config file | Local user | Validated on load; no code execution paths |

### 2.3 Explicitly out of scope

- A local attacker already running as the user. They can read the keyring and the files
  directly; nothing we do inside the process changes that.
- A malicious server serving content the user asked for. We guarantee the bytes are delivered
  faithfully, not that they are safe to run.
- Physical access.

---

## 3. Secret handling

| Rule | Mechanism |
| ---- | --------- |
| Secrets never touch SQLite | Keyring reference stored instead of the value |
| Secrets never appear in logs, at any level | `Secret<T>` newtype rendering `[redacted]` in `Debug`/`Display`; a test greps a `trace`-level run for the material (I-14) |
| Secrets never appear in crash dumps | Zeroised on drop (`zeroize`) |
| Secrets never cross the IPC boundary to clients | Clients receive references and metadata, never values |
| Secrets are deleted with the download | Keyring entry removed on remove/complete |
| Signed-URL query strings are redacted | URLs are logged with the query elided |

Storage: `keyring` crate → Secret Service / kwallet on Linux, Credential Manager on Windows.
If no keyring is available, encrypted-at-rest storage with a key derived from a user
passphrase, and the degradation is stated plainly — never a silent fallback to plaintext.

---

## 4. Privacy

- **Local only.** Download history, URLs, and metadata never leave the machine.
- **No accounts.** Downpour has no sign-in and no server-side component.
- **User control.** History can be cleared per item or entirely; `dp config set
  history.retention_days N` prunes automatically.
- **Extension permissions are minimised.** Each requested permission has a written
  justification in the store listing. `<all_urls>` is requested only if unavoidable, and the
  justification says exactly why.
- **No third-party code at runtime.** No CDN scripts, no remote configuration.

---

## 5. Supply chain

| Control | Practice |
| ------- | -------- |
| Dependency review | Every new crate justified against `14-tech-radar-2026.md`; significant ones get an ADR |
| Advisory scanning | `cargo audit` and `cargo deny` in CI, failing the build |
| Licence compliance | `cargo deny` licence policy; Apache-2.0-compatible only |
| Lockfile | `Cargo.lock` committed for all binaries |
| Reproducible builds | Target for 1.0; documented deviations where not achievable |
| Release signing | GPG for source tarballs; platform signing for installers |
| `unsafe` | Prohibited without a `// SAFETY:` comment; non-trivial blocks need an ADR; `cargo geiger` tracked |

The extension is built from the repository by CI and published from an artifact whose hash is
in the release notes, so a user can verify the store build matches the source.

---

## 6. Vulnerability disclosure

`SECURITY.md` at the repository root:

- Private reporting via GitLab confidential issues, or email. See `SECURITY.md`.
- 90-day coordinated disclosure by default, negotiable for severity.
- Credit given unless the reporter declines.
- No bounty programme (this is an unfunded project), and that is stated honestly rather than
  implied otherwise.

---

## 7. User-facing honesty

The README, the store listings, and the release notes state plainly:

- What Downpour **cannot** do: DRM-protected streams, sites needing bespoke handling, and any
  case where the server refuses parallelism.
- That more connections do not always mean faster, and that the engine deliberately uses fewer
  when more would not help. Users conditioned by other tools expect a large connection count
  to be a feature; explain why it is not.
- That downloading content is subject to the terms of the site and the law of the user's
  jurisdiction, and that Downpour is a transport tool, not permission.

Overstating capability is a support burden and a trust cost. The scorecard in
`00-vision-and-scorecard.md` is published with its gaps intact.
