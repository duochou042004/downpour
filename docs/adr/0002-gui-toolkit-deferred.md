# ADR-0002: GUI toolkit choice deferred to Stage 10

- **Status:** accepted (the decision is *to defer*; the toolkit itself is deferred)
- **Date:** 2026-08-02
- **Stage:** S0, revisited in S10

## Context

The origin discussion named Slint as the UI toolkit. It is a reasonable choice, and it is also
the kind of choice that projects make on day one and regret on day four hundred.

Two things make deferring safe here:

1. The daemon owns all state and the IPC contract is public (ADR-0003). The GUI is a *client*.
   Swapping it costs a rewrite of the GUI crate, not of the system.
2. The GUI is Stage 10. Deciding now means deciding with 2026 information for a build that
   starts much later, in an ecosystem that is moving quickly — Slint and Iced both shipped
   substantial desktop work during 2026.

There is also a licence question that deserves a real decision rather than a default.

## Options

| Option | Licence | Pros | Cons |
| ------ | ------- | ---- | ---- |
| **Iced** 0.14 | MIT | Native `wgpu` rendering, no WebView; reactive rendering; **headless testing** (matters for CI); hot reload; shipping in COSMIC, Halloy, Sniffnet, Kraken Desktop; licence-clean with Apache-2.0 | Tray and native menus need `tray-icon`/`muda`; accessibility via AccessKit is additional work; layout API is less designer-friendly |
| **Slint** 1.17 | `GPL-3.0-only OR Royalty-free OR Commercial` | Strong 2026 desktop push: tray with context menus, modal dialogs, keyboard shortcuts, drag & drop, rich text, tooltips; declarative DSL with tooling; compiles to native code | **The free open-source path is GPL-3.0**, which makes the GUI binary GPL rather than Apache-2.0 (see below) |
| **Tauri** 2.11 | MIT/Apache | Most mature; web frontend | Pulls in WebKitGTK (Linux) / WebView2 (Windows). A browser engine as a runtime dependency for a download manager is the wrong trade — footprint is a scorecard row |
| **egui** | MIT/Apache | Trivially simple, immediate mode | Immediate-mode look and CPU behaviour are wrong for an app that idles most of the day |
| **GTK4 / Qt via bindings** | LGPL / GPL-or-commercial | Native platform integration | Heavy bindings, and the Windows story is the weak half |

### The licence question

`slint` is published as `GPL-3.0-only OR LicenseRef-Slint-Royalty-free-2.0 OR
LicenseRef-Slint-Software-3.0`. For a free and open-source desktop application, the free path
is **GPL-3.0**. Downpour is Apache-2.0 (ADR-0006).

This is workable: the engine crates stay Apache-2.0 and only `downpour-gui` is GPL-3.0. Users
get a GPL binary; downstream projects can still reuse the engine permissively. But it makes
the licence story two paragraphs instead of one word, and it means the GUI cannot be reused by
a permissively licensed project.

Iced has no such issue. That is a real point in its favour, not a decisive one.

## Decision

**Defer the toolkit choice to Stage 10.** Meanwhile:

1. **No GUI code before Stage 10.** Not a prototype, not "just to see". A prototype becomes the
   decision by inertia.
2. **The IPC contract is designed for a GUI from Stage 1** — subscriptions, progress events,
   the segment map, the controller's decision log. If a toolkit-neutral contract can drive a
   GUI, the toolkit is genuinely swappable.
3. **The current leaning is Iced**, on licence cleanliness, native rendering, and headless
   testability. This is a leaning, not a decision.
4. At Stage 10, build the same screen (the queue list, with live progress) in both leading
   candidates, in a spike (`docs/agent/WORKFLOW.md` §"spike work"), and decide on:
   idle CPU and RSS, tray and notification quality on X11/Wayland/Windows, accessibility,
   headless testability, licence, and how the code reads after a week.

## Consequences

**Easier:** the toolkit ecosystem gets four more stages to mature; the decision is made with
evidence from a real spike rather than a comparison table.

**Harder:** no screenshots to show for a long time, which is a real morale and
community-interest cost on an open-source project. Mitigated by making the CLI genuinely good
(`08-ipc-and-ui-spec.md` §4) — `dp explain` is more interesting to the audience this project
will attract early than a window would be.

**Accepted:** we ship a CLI-only tool for a long time.

## Reversal trigger

Bring the decision forward if the IPC contract turns out to need toolkit-specific concessions —
that would mean the abstraction is leaking and the "GUI is just a client" premise is wrong,
which is worth discovering early rather than at Stage 10.
