# Media Specification (HLS / DASH)

**Status: Normative.** Stage 9. Deliberately conservative in scope.

---

## 1. Scope

**In scope:**

- Direct media files (`.mp4`, `.mkv`, `.webm`, `.mp3`, …) — these are ordinary downloads and
  need nothing from this document.
- **Unencrypted** HLS (`.m3u8`) — manifest parsing, variant selection, segment download,
  assembly.
- **Unencrypted** DASH (`.mpd`) — the same.
- Muxing segments into a single container via a remux (stream copy, no re-encode).

**Out of scope, permanently:**

- Any DRM: Widevine, PlayReady, FairPlay, or successors. Not detected-and-warned — not
  attempted.
- Per-site scrapers or extractors. If a site needs bespoke handling, the answer is
  `yt-dlp`, not a copy of it inside Downpour.
- Re-encoding, transcoding, or editing.
- Live stream recording (post-1.0 at the earliest; it is a different problem).

**The AES-128 HLS question.** Some HLS streams are encrypted with a key fetched over HTTPS
with no licence server. This is technically not DRM and is often used only to deter casual
copying. It is nonetheless an access-control measure, and there is no reliable way for the
engine to tell "the site handed the key to my authenticated session" from "I obtained the key
in a way the site did not intend". **Downpour does not decrypt HLS AES-128 in 1.0.** Revisiting
this requires an ADR with a legal analysis, not a code change.

---

## 2. Why media is a separate problem

A normal download is one URL and one byte range space. An adaptive stream is:

- a manifest that names *variants* (resolutions, bitrates, languages),
- each variant a list of segments (hundreds to thousands),
- segments that may be individually short-lived or signed,
- audio and video that may be separate and must be muxed,
- a manifest that can change while you are downloading it.

The engine's interval map does not model that. Media gets its own coordinator that **uses** the
engine per segment.

```
MediaJob
 ├─ manifest fetch + parse
 ├─ variant selection (user or policy)
 ├─ segment plan  ──▶ N ordinary engine downloads (parallel, bounded)
 ├─ integrity + ordering check
 └─ remux ──▶ single output file
```

Each segment is a normal download and gets every guarantee in `04-storage-and-recovery-spec.md`
for free. That is the point of the decomposition.

---

## 3. HLS

### 3.1 Parsing

Master playlist → variant streams. For each variant, record bandwidth, resolution, codecs,
audio group, and language. Media playlist → segment URIs, durations, byte-range subsets
(`#EXT-X-BYTERANGE`), initialisation segment (`#EXT-X-MAP`), discontinuities.

Reject and report, do not guess, when:

- `#EXT-X-KEY` has a `METHOD` other than `NONE` → out of scope (§1).
- `#EXT-X-SESSION-KEY` is present → out of scope.
- The playlist is a live playlist (no `#EXT-X-ENDLIST`) → out of scope for 1.0.

### 3.2 Variant selection

| Mode | Behaviour |
| ---- | --------- |
| `best` (default) | Highest resolution, then highest bandwidth |
| `worst` | Smallest |
| `resolution=1080p` | Nearest at or below |
| `bandwidth<=N` | Highest under the cap |
| `interactive` | Present the list; the user picks |

Audio: default to the variant's default audio group, matching the user's language preference
where one exists. Subtitles: fetch `WebVTT` tracks alongside when present.

### 3.3 Segment download

- Segments are queued as ordinary engine downloads, bounded at `MEDIA_SEGMENT_CONCURRENCY`
  (default 6). Higher does not help — segments are small and per-segment latency dominates.
- `#EXT-X-BYTERANGE` segments share a file; they become range requests against one URL, which
  the engine already does natively.
- Ordering is tracked explicitly. Segments complete out of order; assembly is by index, never
  by completion time.
- A failed segment is retried; after `MAX_SEGMENT_RETRIES` the job fails with the segment
  index reported, so a resume can start from a known point.

---

## 4. DASH

Structurally the same problem with a different manifest.

- Parse `MPD` → periods → adaptation sets → representations → segments (`SegmentTemplate`,
  `SegmentList`, or `SegmentBase` with index ranges).
- Reject `ContentProtection` elements → out of scope (§1).
- Multi-period manifests: each period is downloaded and concatenated in order; a discontinuity
  between periods is preserved as a container boundary rather than smoothed over.
- `SegmentTemplate` with `$Number$` / `$Time$` substitution must be expanded exactly; an
  off-by-one here produces a file that plays with a glitch, which is the kind of bug that
  survives casual testing.

---

## 5. Assembly

```
1. Verify every planned segment is present and its length matches the plan.
2. Verify ordering by index. Never by filename or by completion order.
3. If the segments are already a single container (fMP4 with an init segment):
      concatenate init + segments → output.
4. Otherwise remux with FFmpeg: -c copy, no re-encode.
5. Verify the output: duration within tolerance of the manifest total, stream count matches.
6. Atomic rename to the final name (I-4).
7. Delete segment files only after step 6 succeeds.
```

### FFmpeg dependency

FFmpeg is used for remux only, and it is **optional**:

- If FFmpeg is absent, single-container cases (fMP4) still work by concatenation.
- Cases needing a remux report clearly that FFmpeg is required, with the install command for
  the user's platform.
- FFmpeg is invoked as a subprocess with an explicit argument vector — never a shell string.
  Manifest-derived values reach it as arguments, never as shell text.
- We do not bundle FFmpeg in the default packages (licensing and size). Flatpak and the
  Windows installer may offer it as an optional component.

---

## 6. Resumability

A media job is resumable at segment granularity:

- The plan (manifest snapshot, variant choice, segment list) is persisted at job start.
- Completed segments are recorded; resume re-plans only what is missing.
- If the manifest has changed since the snapshot (segments renamed, list shifted), the job
  **stops and asks** rather than mixing two versions of the stream. Same principle as I-3.

---

## 7. Testing

- Manifest parser: property tests over generated manifests; a corpus of real-world manifest
  shapes (multi-period, byte-range, discontinuity, `$Time$` templates) collected from
  standards test vectors and our own generated servers.
- Segment ordering: a simulation that completes segments in adversarial orders and asserts the
  assembled output is byte-identical to sequential assembly.
- Assembly: compare Downpour's output against a reference `ffmpeg` run on the same inputs;
  byte-identical for concatenation cases, stream-identical for remux cases.
- Rejection paths: every DRM and live-stream case must be *rejected*, and there must be a test
  asserting the rejection. A regression that starts accepting an encrypted stream is a
  boundary violation, and it should fail CI loudly.
