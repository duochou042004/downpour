# Vision and Scorecard

**Status: Normative.** The scorecard defines what "done" means. Changing a weight or a
threshold requires an ADR.

---

## 1. The goal, stated precisely

> If IDM is 10/10, Downpour ships at 9.5/10 or it does not ship.

That sentence is only useful if 9.5 is a number rather than a feeling. This document makes it
one.

**What Downpour is:** a download manager for Linux and Windows that a person who currently
pays for IDM would switch to without feeling they downgraded — and that a person who cannot
run IDM has never had at all.

**What Downpour is not:** a torrent client, a media ripper for protected services, a browser,
or a general-purpose network tool. Scope discipline is the difference between shipping and
not shipping.

---

## 2. The scorecard

Six categories, weighted. A release is scored against this table with evidence per row.

| # | Category | Weight | What is measured |
| - | -------- | -----: | ---------------- |
| 1 | **Correctness — no corrupted files** | 25 | Silent corruption count across corpus + simulation. Must be zero. |
| 2 | **Resume, crash recovery, URL refresh** | 20 | Recovery success rate across the crash matrix; correct refusal on representation change. |
| 3 | **Speed and bandwidth utilisation** | 20 | Time-to-first-byte, time-to-90%-of-link, mean throughput, tail-segment latency, versus IDM/AB/aria2/browser. |
| 4 | **Server, proxy, and auth compatibility** | 15 | Corpus pass rate across the pathology matrix. |
| 5 | **Browser integration and media detection** | 15 | Capture reliability on Chrome/Edge/Firefox; HLS/DASH (non-DRM) detection and assembly accuracy. |
| 6 | **Resource use, UX, packaging** | 5 | RSS and CPU under a 20-item queue; install/update experience on a clean machine. |

**Total: 100. Target: ≥ 95.**

### Category 1 is not tradeable

Category 1 does not average out with the others. **Any silent corruption finding is an
automatic release block**, regardless of the other 75 points. A download manager that is fast
and occasionally hands you a broken ISO is worse than useless, because the user does not find
out until much later and does not know to blame it.

This is the single most important sentence in the specification.

### Scoring rules

- Each row is scored 0–100% of its weight, with evidence that another person can reproduce.
- "Partially working" scores as the fraction that actually works, not as "mostly there".
- A category with no evidence scores **zero**, not "assumed fine".
- Comparative rows (3, 5) are scored against measured competitors on the local test matrix,
  never against remembered impressions or marketing claims.

---

## 3. What 9.5 requires, concretely

These are the release gates that make the number real. Each maps to a scorecard row.

| Gate | Requirement | Row |
| ---- | ----------- | --- |
| G1 | Zero silent corruption across the entire corpus and every simulation seed. | 1 |
| G2 | Survives crash injection at **every** write boundary, on Linux and Windows, resuming to a byte-correct file. | 1, 2 |
| G3 | Resumes correctly when the validator is unchanged; **refuses and reports** when it changed. Never splices. | 2 |
| G4 | An expired signed URL can be replaced with a fresh one and the transfer continues from the existing bytes. | 2 |
| G5 | Meets or beats IDM on the majority of accelerable test cases. Ties where the link is saturated are ties, not losses. | 3 |
| G6 | Never makes a download *slower* than a single connection would have been. Adaptive concurrency must be able to reach 1. | 3 |
| G7 | ≥ 95% pass rate on the compatibility corpus, with every failure documented as a known limitation. | 4 |
| G8 | Browser capture works on current Chrome, Edge, and Firefox, including download context (cookies, referer, headers). | 5 |
| G9 | Non-DRM HLS and DASH are detected and assembled byte-correctly, verified against a reference remux. | 5 |
| G10 | Idle RSS under 60 MB; RSS under a 20-download queue under 250 MB; idle CPU effectively zero. | 6 |
| G11 | Installs, updates, and uninstalls cleanly on a fresh Debian/Ubuntu/Fedora/Arch and Windows 11 machine. | 6 |

G10's numbers are targets to be validated in Stage 10 and adjusted by ADR if they turn out to
be wrong. They exist so that "low footprint" is a measurement rather than a claim.

---

## 4. Where the 0.5 goes

Being honest about what we will not match is part of taking the target seriously.

Downpour will likely never equal IDM on:

- **Per-site bespoke handling.** IDM ships handling for specific players and specific sites,
  accumulated over two decades. We will cover the general cases well and the long tail poorly.
- **Windows shell integration depth.** IDM's integration with legacy Windows browsers and
  applications is deeper than a modern cross-platform app will replicate.
- **Protected streams.** IDM does not break DRM either, but it has more coverage of the
  awkward middle ground. We are deliberately conservative here (see
  `10-security-privacy-legal.md`).

Downpour should exceed IDM on:

- **Protocol generation.** HTTP/2 and HTTP/3 stream-parallel transfer, DNS-based protocol
  discovery, 0-RTT resumption. IDM's architecture predates all of it.
- **Adaptive behaviour.** Measuring the environment instead of applying a fixed connection count.
- **Verifiable correctness.** A published corpus, deterministic simulation, and crash
  injection that anyone can run. IDM's correctness is a matter of reputation; ours will be a
  matter of a test suite people can execute.
- **Platform coverage.** Linux as a first-class target, not an absence.
- **Openness.** Apache-2.0, auditable, no licence key, no phone-home.

---

## 5. Non-goals

Explicitly out of scope. Adding any of these requires an ADR arguing why the scope change is
worth the delay to the gates above.

- BitTorrent / magnet links
- FTP / SFTP *(candidate for post-1.0)*
- Cloud sync of the download queue
- A built-in browser
- Site-specific scrapers or account grabbers
- Intercepting every network request the browser makes
- Any form of DRM circumvention
- Mobile (Android/iOS) clients before 1.0

---

## 6. Why this can work

The reasonable objection is: *IDM has twenty years of accumulated compatibility knowledge;
you cannot catch up.*

That is true, and it is why the strategy is not to catch up by writing more code. It is:

1. **Change the protocol generation.** A large fraction of IDM's accumulated handling
   addresses HTTP/1.1-era problems. HTTP/2 and HTTP/3 make some of those problems disappear
   and create a smaller set of new ones. We start on the new set.
2. **Automate the corpus.** IDM's knowledge was accumulated by users hitting broken servers
   and reporting them. We generate the pathologies deliberately, in a lab, from the RFCs and
   from a taxonomy of what servers actually get wrong — and we can generate thousands of
   combinations in the time it takes to encounter one in the wild.
3. **Make correctness mechanical.** Property tests and deterministic simulation catch the
   class of bug that IDM had to find in production. A failing seed is a permanent, exactly
   reproducible test case.

The corpus is the product. The engine is what runs against it.

---

## 7. How this document is used

- At every stage gate, the relevant scorecard rows are estimated and the estimate is recorded
  in `state/progress.json` under `scorecard`.
- Before any release, the full scorecard is scored with evidence and published.
- If a category is consistently scoring low across stages, that is the signal to change the
  plan — not to lower the weight.
