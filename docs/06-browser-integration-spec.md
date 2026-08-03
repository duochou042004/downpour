# Browser Integration Specification

**Status: Normative.** Stage 7.

Per `01-idm-teardown.md` §2.1, this is the highest-value feature in the product. An engine
without browser context is a `curl` with a queue; an engine *with* it works on the sites where
people actually need a download manager.

---

## 1. Scope

The extension does four things and nothing else:

1. Intercept downloads the user initiates and hand the URL **plus its request context** to the
   daemon.
2. Collect the context: referer, tab URL, the cookies scoped to that request, request headers,
   method and body where applicable.
3. Observe media requests (`.m3u8`, `.mpd`, direct media responses) and offer them.
4. Render a download button on pages where media was detected.

Explicit non-goals: no page modification beyond the button, no analytics, no remote code, no
observing requests unrelated to downloads.

---

## 2. Manifest V3 realities

Chrome enforces MV3; MV2 is finished. The relevant consequences:

| Constraint | Effect | Our response |
| ---------- | ------ | ------------ |
| Background pages → terminating service workers | No long-lived in-memory state | All state in the daemon. The worker is stateless and reconstructs from `chrome.storage` on wake. |
| `webRequest` blocking removed | Cannot cancel/rewrite a request in flight | We do not need to. Observation (`webRequest` non-blocking) is retained and is sufficient for detection. |
| `declarativeNetRequest` for blocking | Static rule sets | Not needed — we are not a blocker. |
| No remotely hosted code | Everything ships in the package | Fine; we ship a small bundle. |

The `chrome.downloads` API is the primary capture path: `chrome.downloads.onDeterminingFilename`
and `onCreated` give us the URL, the referrer, the suggested filename, and the MIME type, and
`chrome.downloads.cancel` lets us take the download over. `webRequest` observation supplements
it for media that never becomes a browser download.

**Firefox** implements MV3 while retaining more of `webRequest`. The extension is written to a
common core with a thin per-browser adapter, not forked.

---

## 3. Native messaging

The only channel between the extension and the daemon. There is deliberately **no localhost
HTTP server** — an unauthenticated local port is a well-known hole and it is not needed.

```
extension ──stdio, 4-byte LE length prefix + JSON (max 1 MB)──▶ downpour-host
downpour-host ──JSON-RPC 2.0 over UDS / named pipe──▶ downpourd
```

### 3.1 `downpour-host`

Thin, stateless, and hostile-input-aware (I-13):

- Reads a length-prefixed frame; **rejects** any frame over 1 MB without allocating for it.
- Validates every message against the schema before any field is used.
- Translates to the daemon's JSON-RPC and relays the response.
- Starts the daemon if it is not running, then retries once.
- Makes no network requests of its own, ever. If the host can be made to fetch a URL, the
  browser sandbox has been usefully extended for an attacker.
- Logs to its own file, never to stdout — stdout is the protocol channel.

### 3.2 Manifest registration

| Browser | Linux | Windows |
| ------- | ----- | ------- |
| Chrome/Chromium | `~/.config/google-chrome/NativeMessagingHosts/com.downpour.host.json` | Registry `HKCU\Software\Google\Chrome\NativeMessagingHosts\com.downpour.host` |
| Firefox | `~/.mozilla/native-messaging-hosts/com.downpour.host.json` | `HKCU\Software\Mozilla\NativeMessagingHosts\com.downpour.host` |

The manifest pins `allowed_origins` (Chrome) / `allowed_extensions` (Firefox) to our published
extension IDs. Any other extension talking to the host is rejected.

---

## 4. The capture flow

```
User clicks a download link
        │
        ▼
chrome.downloads.onCreated fires
        │
        ├─ Does it match the user's capture rules?   no → let the browser handle it
        │
        ▼ yes
Collect context:
   - url, finalUrl, referrer, tab url, tab title
   - cookies for the request URL (cookies.getAll, scoped)
   - suggested filename, mime, file size if known
   - request headers observed via webRequest for this request id
        │
        ▼
chrome.downloads.cancel(id)          ← take it over
        │
        ▼
Send to native host → daemon → download queued
        │
        ▼
Show a confirmation the user can undo
```

### 4.1 Capture rules

Off by default is wrong (nobody would find it); capturing everything is also wrong (breaks
webmail attachments, in-page previews, blobs). Default rules:

| Rule | Default |
| ---- | ------- |
| File extension list | Common archives, disk images, installers, media, documents |
| Minimum size | 1 MB when the size is known |
| Content-Type list | `application/octet-stream`, `application/zip`, `video/*`, `audio/*`, … |
| Domain allow/deny list | Empty; user-populated |
| `blob:` and `data:` URLs | Never captured — nothing to re-request |
| Same-page navigations, XHR, fetch | Never captured |
| Manual override | The context menu always offers "Download with Downpour" |

Every rule is user-editable. Getting this wrong is the top source of "download manager
extension is annoying" complaints.

### 4.2 The cookie question

Cookies are the mechanism that makes downloads work on authenticated sites, and they are also
credentials. Rules:

- Collect only cookies whose scope actually matches the request URL. Never the whole jar.
- Transmit over native messaging only (never over a network socket).
- The daemon stores them in the OS keyring, referenced by download id, never in SQLite (I-14).
- Redact from every log at every level.
- Delete when the download reaches `Completed` or `Failed` and is removed.
- Chrome does not expose `HttpOnly` cookies to `cookies.getAll`. Where they are required, the
  download will fail with an auth error, and the correct handling is to tell the user plainly
  rather than to attempt a workaround.

---

## 5. Media detection

Observe (non-blocking) and classify:

| Observation | Classification |
| ----------- | -------------- |
| `.m3u8` response, or `application/vnd.apple.mpegurl` | HLS manifest |
| `.mpd` response, or `application/dash+xml` | DASH manifest |
| Large `video/*` or `audio/*` response with range support | Direct media |
| `.ts` / `.m4s` segments without a manifest | Segmented stream, manifest not seen — offer only if the manifest is later found |
| Response with an EME/DRM signal on the page | **Not offered.** See §7. |

Detected media is grouped per tab and surfaced in the extension's popup and, where the page
has a recognisable player container, as an unobtrusive button. Assembly is the daemon's job
(`07-media-spec.md`).

---

## 6. Extension ↔ host message schema

Versioned, and every message is validated before use.

```typescript
type ToHost =
  | { v: 1; kind: "ping" }
  | { v: 1; kind: "capture"; payload: CaptureRequest }
  | { v: 1; kind: "media"; payload: MediaCandidate }
  | { v: 1; kind: "refresh"; payload: { downloadId: string; url: string; context: RequestContext } }
  | { v: 1; kind: "list" }
  | { v: 1; kind: "subscribe"; payload: { downloadIds: string[] } };

interface CaptureRequest {
  url: string;
  finalUrl?: string;
  referrer?: string;
  pageUrl?: string;
  pageTitle?: string;
  filename?: string;
  mimeType?: string;
  fileSize?: number;
  method: "GET" | "POST";
  headers: Record<string, string>;   // allow-listed names only
  body?: string;                     // base64, POST only, size-capped
  cookies: Array<{ name: string; value: string; domain: string; path: string }>;
}
```

Header collection is **allow-listed**, not deny-listed: `Referer`, `Origin`, `User-Agent`,
`Accept`, `Accept-Language`, `Range`, `Authorization`, and a configurable extra list.
Forwarding everything the browser sends is how you leak something you did not intend to.

---

## 7. Boundaries

Restating `10-security-privacy-legal.md` because this is where the temptation is:

- **No DRM circumvention.** If the page uses EME (Widevine, PlayReady, FairPlay), the media is
  not offered. Not "offered with a warning" — not offered.
- **No TLS interception.** No MITM proxy, no injected root CA, not opt-in.
- **No credential harvesting.** Only cookies scoped to a request the user explicitly started.
- **No telemetry.** The extension makes no network requests of its own. Its only outbound
  channel is the native messaging port.
- **No page modification** beyond the download button in a shadow root.

These are what make the difference between an integration and a piece of spyware, and they are
the first things a reviewer at a browser store will look for.

---

## 8. Automatic URL refresh

The feature IDM does manually, done automatically — the single most user-visible improvement
available.

```
daemon: download D hits 403 → AwaitingRefresh
        │
        ▼ event to the extension
extension: is the origin page still open in a tab?
        │
        ├─ yes → re-trigger the download in that page context,
        │        capture the new signed URL, send `refresh`
        │
        └─ no  → notify: "Downpour needs a fresh link for <file>.
                  Open <page> and start the download again."
        │
        ▼
daemon: probe the new URL, check compatibility (engine spec §5.2),
        rebind, continue from the existing bytes
```

The compatibility check is mandatory. Rebinding to a different file because the URL looked
similar is exactly the corruption I-3 exists to prevent.

---

## 9. Testing

- Extension unit tests with mocked `chrome.*` APIs.
- Native host: fuzz the frame decoder (`cargo-fuzz`); corpus of malformed frames including
  oversized length prefixes, truncated bodies, and deeply nested JSON.
- End-to-end with a real browser under Playwright against the local corpus server: capture,
  cancel-and-take-over, media detection, refresh.
- **A manual matrix is unavoidable** for current Chrome, Edge, and Firefox on both platforms.
  Record it in the release checklist; browser behaviour changes without notice, and this is
  the part that cannot be fully automated.
