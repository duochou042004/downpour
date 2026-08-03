# Context Map — which document to read for which task

`docs/` is large on purpose. Reading all of it for a small change wastes the context you need
for the work. Find your task below, read the **Read** column, skim the **Also** column only if
the task turns out to touch it.

Always read first, regardless of task: `docs/agent/HARNESS.md`, `state/progress.json`.

---

## By task type

| Your task | Read | Also |
| --------- | ---- | ---- |
| Probe a URL, decide whether ranges work | `03-transfer-engine-spec.md` §2, `05-protocol-matrix.md` | `INVARIANTS.md` I-5, I-6 |
| Split, merge, or reassign segments | `03-transfer-engine-spec.md` §4, `INVARIANTS.md` I-2 | `09-testing-strategy.md` §2 |
| Change how many connections/streams are opened | `03-transfer-engine-spec.md` §3 | `05-protocol-matrix.md`, `INVARIANTS.md` I-7 |
| Write bytes to disk, flush, or preallocate | `04-storage-and-recovery-spec.md` | `INVARIANTS.md` I-1, I-10 |
| Anything about pause, resume, or crash recovery | `04-storage-and-recovery-spec.md` §3–§5 | `INVARIANTS.md` I-1, I-3, I-9 |
| Handle an expired or rotated URL | `03-transfer-engine-spec.md` §5 | `INVARIANTS.md` I-8 |
| Add or change an HTTP protocol backend | `05-protocol-matrix.md`, `03-transfer-engine-spec.md` §6 | `14-tech-radar-2026.md`, ADR-0005 |
| Work on the browser extension | `06-browser-integration-spec.md` | `10-security-privacy-legal.md` |
| Work on the native messaging host | `06-browser-integration-spec.md` §3, `08-ipc-and-ui-spec.md` §2 | `INVARIANTS.md` I-13 |
| Detect or assemble HLS / DASH | `07-media-spec.md` | `10-security-privacy-legal.md` §2 |
| Change an IPC message | `08-ipc-and-ui-spec.md` | `INVARIANTS.md` I-11, I-12 |
| Build any part of the desktop UI | `08-ipc-and-ui-spec.md` §4, ADR-0002 | `12-roadmap-stages.md` S10 |
| Write a test of any kind | `09-testing-strategy.md` | the spec for the thing under test |
| Add a compatibility corpus case | `09-testing-strategy.md` §2 | `01-idm-teardown.md` |
| Anything touching cookies, auth, credentials | `10-security-privacy-legal.md` | `INVARIANTS.md` I-14 |
| Packaging, installers, updates, signing | `11-packaging-release.md` | `12-roadmap-stages.md` S10 |
| Pick or upgrade a dependency | `14-tech-radar-2026.md` | `docs/adr/` |
| Decide whether we are "done enough" | `00-vision-and-scorecard.md` | `12-roadmap-stages.md` |
| Understand why a past choice was made | `docs/adr/README.md` (index) | the specific ADR |
| Understand what IDM actually does | `01-idm-teardown.md` | `docs/reference/00-origin-conversation.md` |

---

## By stage

Each stage has a primary document. If you are working "on Stage N", this is the one to hold
in context.

| Stage | Primary document |
| ----- | ---------------- |
| S0 Foundations | `12-roadmap-stages.md`, `docs/adr/` |
| S1 Single-stream downloader | `03-transfer-engine-spec.md` §1–§2 |
| S2 Range, resume, crash safety | `04-storage-and-recovery-spec.md` |
| S3 Dynamic segmentation | `03-transfer-engine-spec.md` §4 |
| S4 Adaptive concurrency | `03-transfer-engine-spec.md` §3 |
| S5 HTTP/2 streams | `05-protocol-matrix.md` |
| S6 HTTP/3 / QUIC | `05-protocol-matrix.md`, ADR-0005 |
| S7 Browser integration | `06-browser-integration-spec.md` |
| S8 Auth, cookies, URL refresh | `03-transfer-engine-spec.md` §5, `10-security-privacy-legal.md` |
| S9 HLS / DASH | `07-media-spec.md` |
| S10 UI, queue, packaging | `08-ipc-and-ui-spec.md`, `11-packaging-release.md` |

---

## Document status legend

| Marker | Meaning |
| ------ | ------- |
| **Normative** | The implementation must match this. Deviating requires an ADR. |
| **Informative** | Background and rationale. Useful, but not a contract. |
| **Living** | Expected to change as the project learns. Update it when reality diverges. |

| Document | Status |
| -------- | ------ |
| `00-vision-and-scorecard.md` | Normative (the scorecard defines "done") |
| `01-idm-teardown.md` | Informative |
| `02-architecture.md` | Normative |
| `03-transfer-engine-spec.md` | Normative |
| `04-storage-and-recovery-spec.md` | Normative |
| `05-protocol-matrix.md` | Normative |
| `06-browser-integration-spec.md` | Normative |
| `07-media-spec.md` | Normative |
| `08-ipc-and-ui-spec.md` | Normative |
| `09-testing-strategy.md` | Normative |
| `10-security-privacy-legal.md` | Normative (boundaries are absolute) |
| `11-packaging-release.md` | Normative |
| `12-roadmap-stages.md` | Living |
| `13-glossary.md` | Informative |
| `14-tech-radar-2026.md` | Living |
| `docs/agent/INVARIANTS.md` | Normative (violations block release) |
| `docs/agent/HARNESS.md` | Normative (process) |
| `docs/reference/*` | Informative (historical) |
