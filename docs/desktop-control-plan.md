# Desktop control when content cannot be captured

Status: PROPOSED implementation plan, 2026-11-04. Extends the Phase-4 acceptance criteria; does not authorize starting Phase 4 before the current milestone. No compatibility results are claimed.

## Product requirement

One application with unavailable or protected content must not disable JEAN's desktop session. JEAN must continue independent tasks in other applications and use supported nonvisual controls where available. A blocked read is not automatically a blocked launch, navigation, or action.

Telegram is an initial compatibility target, not a special-case workaround. Test its installed Windows client and version before claiming support. A private chat and a capture-protected chat are different cases.

## What the platform supports

- Windows display affinity can make an individual window blank or exclude it from capture. This does not establish that every other window is inaccessible. [Microsoft: SetWindowDisplayAffinity](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-setwindowdisplayaffinity)
- Windows Graphics Capture supports targeting a particular window. Per-window capture isolates work on another application; it does not remove the selected application's protection. [Microsoft: CreateForWindow](https://learn.microsoft.com/en-us/windows/win32/api/windows.graphics.capture.interop/nf-windows-graphics-capture-interop-igraphicscaptureiteminterop-createforwindow)
- Telegram explicitly supports content protection, including for private chats. [Telegram: content protection](https://core.telegram.org/api/content-protection)
- UNVERIFIED: which Telegram controls and messages the user's installed client exposes through UI Automation, and which capture failures reproduce on this machine. Do not infer these from protocol documentation.

## Independent capability checks

Keep window discovery, content reading, action execution, and outcome verification separate. Maintain observations per application/window and task, not a single desktop-wide `can_see` flag.

For each requested operation, record whether there is a usable app API, accessibility control, capture, input path, and verifier. Include source, timestamp, target identity and failure reason. Invalidate observations after navigation, window recreation, session changes, or application updates.

Distinguish at least: available, unsupported, permission denied, explicit protection, temporarily unavailable, and unknown. A black frame or an empty accessibility tree is evidence of unavailable content, not proof of protection.

## Execution strategy

1. Resolve the exact application, process, window and requested action. Revalidate identity before acting; handles and focus can change.
2. Prefer a supported application API or connector when configured and authorized. An API integration needs its own authenticated session and permissions; a logged-in desktop is not automatic API authorization.
3. Inspect UI Automation for the required controls and patterns. Screenshot failure alone does not disqualify accessible controls. Do not assume that message text is exposed just because window buttons are exposed.
4. Capture the relevant window on demand if visual context is required and permitted. Do not make full-monitor capture a prerequisite for unrelated work.
5. Use keyboard/input actions only when the target, focus, operation and verification path are established. Do not blindly click inferred content or type into an unverified recipient.
6. If the operation lacks a supported observation/action path, pause only its dependent steps and request a manual handoff. Continue independent tasks and keep cancellation and status responsive.

An explicit restriction on content access is not a cue to extract that content through another path. Use connectors within the app's permissions and content rules. No injection, memory scraping, patched clients, or protection-disabling fallback is planned.

## Keep the rest of the desktop usable

- Supervise capture and accessibility work outside the core with timeouts. A hung provider must not freeze scheduling, voice, cancellation, or another application's worker.
- On a failed or suspicious full-display capture, enumerate windows independently and try capture of the unrelated target window. Refresh its accessible state before acting.
- An excluded overlay may leave misleading imagery in a desktop capture. Check the actual foreground/window at the input target; do not click a visually exposed window underneath a real foreground window.
- If Telegram covers another target, activate that target when task authorization permits. Moving/minimizing Telegram is optional workspace management, not a way to expose Telegram's protected content. Verify focus changes and respect foreground-lock failures.
- Bound capture recovery: one retry after checking target state, then one alternate supported backend for ordinary compatibility failures. Do not cycle backends to defeat explicit protection. Escalate unresolved failures with evidence instead of retrying forever.
- Lock screen, secure desktop, disconnected sessions, minimized windows, stale frames, graphics failures, and explicit protection need distinct diagnostics. Pause affected GUI tasks when the interactive session is unavailable; do not promise uninterrupted GUI execution through those conditions.

## Telegram support levels

| Operation | Planned approach | Evidence required |
|---|---|---|
| Discover/open/focus Telegram | Windows process and window integration | Correct process/window exists; focus verified where needed |
| Navigate exposed app controls | UIA patterns or documented shortcuts | Expected accessible state and target identity |
| Read ordinary permitted messages | UIA if exposed; otherwise a separately authorized connector or capture | Actual returned content with known source; no assumption of API/desktop parity |
| Work with protected content | Respect content rules; manual handoff where access is unavailable | Explicit user completion or independently observable outcome |
| Draft/send a message | Compose a draft and confirm recipient/content under the action policy | Sending is never inferred from typing; use an observable send result, or report unverified |
| Continue another app's task while Telegram is open | Target-specific API/UIA/capture | That task succeeds without needing Telegram's content |

An API connector is optional future work, not an adopted dependency. Do not silently switch to a browser, account, client or chat to obtain different access. Do not read private chats, send test messages, or collect their contents for compatibility testing without explicit task authorization.

## Acceptance tests

First build a synthetic Windows fixture with normal, blank, hung, and capture-excluded windows and known UIA controls. Use it to verify isolation without involving private data.

Then test opt-in Telegram scenarios using a user-owned test conversation and harmless content. Record Windows version, Telegram client/version, capture backend, integrity level, DPI, and session state. Other applications require their own compatibility results.

Required demonstrations:

1. Telegram/protected fixture open, overlapping and maximized: complete a known task in Notepad and an ordinary browser window; verify actual state changes.
2. Protected fixture content absent from capture: exposed permitted UIA controls still operate and verify correctly, without extracting restricted content.
3. Black image, stale image, minimized window and explicit protection are not all mislabeled as the same failure.
4. A hung capture/UIA operation times out while another application task and cancellation remain responsive.
5. Unexpected focus/recipient changes prevent typing or sending. No blind input based on stale screenshots.
6. No supported path: task enters `needs_attention`, independent work continues, and an explicit handoff permits replanning without replaying completed side effects.
7. Protected/private content does not enter diagnostic logs, saved screenshots, or cloud requests as a side effect of probing. Log capability outcomes and necessary metadata instead.

## Delivery sequence

1. Finish the current Phase-0 stabilization; no desktop-control code belongs in that batch.
2. At the beginning of Phase 4, prototype window discovery, per-window capture and UIA against the fixture and installed Telegram client. Establish an operation-level compatibility matrix before adding OCR/VLM fallback.
3. Implement worker timeouts, independent capability observations, focus checks, bounded recovery and manual handoff.
4. Demonstrate the acceptance tests, then update Phase-4 roadmap criteria and record accepted architecture/dependency decisions. Keep this plan proposed until those decisions are made.

The deliverable is resilient desktop control with explicit supported operations, not a promise that every application's protected content can be read.
