---
name: probe-design
description: Design or review a Downpour capability probe — the ranged GET that discovers what a server will actually allow, and the validation that decides whether segmentation is permitted. Use when implementing, extending, or debugging the probe.
when_to_use: Working on capability detection; a server is misclassified; adding a protocol backend that needs its own probe path.
argument-hint: "[the server behaviour or backend in question]"
allowed-tools: Read, Grep, Glob, Edit, Write, Bash
---

# Capability probe design

The probe answers one question: *what will this server actually let us do?* It never trusts
advertisement. Getting it wrong means either an unnecessary single-stream download (slow) or a
segmented download against a server that does not support it (corrupt).

Read `docs/03-transfer-engine-spec.md` §2 first, and invariants I-5 and I-6.

## The non-negotiables

1. **Ranged GET, not HEAD.** `HEAD` is a supplementary signal only. Servers routinely answer it
   differently from `GET`, and CDN edges differ from origins.
2. **`Accept-Encoding: identity` on every ranged request** (I-5). A compressed response makes
   the bytes on the wire stop corresponding to the byte range that was asked for.
3. **`range_support: Proven` is set only by an observed, validated `206`** (I-6). Nothing else
   sets it. Not `Accept-Ranges`. Not a successful `HEAD`.
4. **Validate `Content-Range` against what was requested.** Parse it, then check it. A
   `Content-Range` that disagrees with the request means reject the response — do not write it.
5. **Sanity-check the body.** An HTML login page returned with `200` where a binary was expected
   is a session problem, not content. Check content type and magic bytes before accepting bytes.

## Checklist for a probe implementation

- [ ] `Range: bytes=0-0`, `Accept-Encoding: identity`
- [ ] Redirects followed; the **entire chain** and the final URL recorded (I-8)
- [ ] Status is `206`, or range support is Absent
- [ ] `Content-Range` present, syntactically valid, and matching the request
- [ ] Total length parsed from `Content-Range`; absent → no segmentation
- [ ] Body is exactly one byte; more means the server ignored the range
- [ ] `Content-Encoding` absent or `identity`, else reject
- [ ] Strong `ETag` recorded; a weak (`W/"…"`) ETag recorded as "no strong validator"
- [ ] `Repr-Digest` / `Content-Digest` recorded if offered (RFC 9530 — free integrity)
- [ ] `Content-Disposition` filename extracted and sanitised
- [ ] Content-type / magic-byte sanity check
- [ ] Result cached per (origin, proxy), not per origin alone
- [ ] Re-probe triggers implemented: URL change, inconsistent worker response, stale freshness
      window, user-supplied refresh

## Corpus cases a probe change should have

`accept-ranges-lies`, `head-differs-from-get`, `content-range-mismatch`,
`content-range-absent`, `gzip-on-range`, `cdn-edge-disagrees`, `no-content-length`,
`html-instead-of-file`, `weak-etag-only`, `redirect-chain-to-other-host`.

If your change touches a path none of these exercise, write the case that does.
