---
name: rfc-check
description: Verify a protocol assumption against the normative RFC text before it becomes a bug in the Downpour engine. Use whenever code or a spec relies on "the server will..." or "HTTP guarantees..." and that claim has not been checked against the actual requirement level.
when_to_use: About to write code that assumes a server behaviour; reviewing a claim about HTTP, TLS, QUIC or DNS; a corpus case's expected behaviour is disputed.
argument-hint: "[the assumption to check]"
allowed-tools: Read, Grep, WebSearch, WebFetch
---

# Check an assumption against the RFC

Most download-manager compatibility bugs are a MAY read as a MUST. `Accept-Ranges` is the
canonical example: it is advisory, servers advertise it and then ignore ranges, and an engine
that treats it as a guarantee downloads the file N times and assembles nonsense.

## Procedure

1. **State the assumption precisely.** "The server will return 206 for a range request" is
   vague. "A server that sent `Accept-Ranges: bytes` on a HEAD will return 206 for a subsequent
   ranged GET of the same resource" is checkable.

2. **Find the normative text.**

   | Topic | Document |
   | ----- | -------- |
   | HTTP semantics, ranges, conditionals, validators | RFC 9110 |
   | HTTP caching | RFC 9111 |
   | HTTP/1.1 message syntax | RFC 9112 |
   | HTTP/2 | RFC 9113 |
   | HTTP/3 | RFC 9114 |
   | QUIC transport | RFC 9000 |
   | SVCB / HTTPS DNS records | RFC 9460 |
   | Digest fields | RFC 9530 |
   | Content-Disposition | RFC 6266 |
   | TLS 1.3 | RFC 8446 |

3. **Quote the sentence.** Not a paraphrase. Paraphrasing from memory is how subtly wrong
   behaviour enters an engine and stays there for two years.

4. **Classify the requirement level:** MUST / SHOULD / MAY / optional / undefined.

5. **Check what implementations actually do.** Where the spec says SHOULD or MAY, field
   behaviour is what matters. Servers, CDNs, and corporate proxies all diverge.

6. **Decide what Downpour does**, and whether it needs a corpus case.

## Output

```markdown
**Assumption:** <as stated>
**Verdict:** safe | unsafe | conditionally safe

**Normative basis:** RFC 9110 §14.3 — "<quoted sentence>"
**Requirement level:** MAY

**Field reality:** <what implementations do, and how often they diverge>

**Downpour must:** <concrete behaviour>
**Corpus cases implied:** <ids>
**Invariant:** <if this maps to one>
```

## Rule

If you cannot find normative text, say so. "I could not confirm this" is a useful answer; a
confident guess about protocol behaviour is a compatibility bug with a delay fuse.
