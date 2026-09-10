# Safari browser connector

Standalone Rust stdio MCP connector for macOS Safari, using the existing normal
Safari profile in one connector-owned window. Safari owns page and login state;
the connector retains only its window identifier. This optional tool has its own
Cargo workspace and lockfile, matching the other local macOS helpers.

## Browser operations

Register the executable as the `safari_browser` MCP server to expose these names
with the `safari_browser__` prefix:

| Tool | Operations |
| --- | --- |
| `browse` | Open/read/scroll a public HTTPS page, wait for an expected control, verify an expected field value, or request a display wake. |
| `interact` | Click, fill, select a native/ARIA option, check/uncheck, activate a link/button with Enter, set a native date, or use native AutoFill. |
| `close` | Close only the connector-owned window. |

Open, scroll and interactions return updated page state. Inspect that result
before another action. Use the exact selectors from the latest controls;
ambiguous, hidden, disabled, and read-only mutation targets fail explicitly.

`open` and `read` accept `expected_selector`; `wait` accepts `selector`. They poll
for document completion, a visible enabled expected control, and no visible
`aria-busy=true` element, then require a stable page fingerprint for 600ms. The
bounded polling budget is `timeout_ms` (100–10000ms, default 5000ms). A
`readiness.status` of `timed_out` is incomplete observation, not proof that a
control or saved value is absent. Generic document readiness cannot discover
an application's unstated loading conditions: supply its expected control.
Each wait has its own deadline; native automation and link-policy work can add
to total tool duration. Invalid or ambiguous expected selectors are errors.
Opening a reused window validates its current native URL before marking its old
document, then requires the exact document URL to still match that preflight.
It waits for replacement before reading; a previously complete form cannot
satisfy a pending reopen. Exact
fragment-only navigations are handled separately. Redirected URLs retain the
same public-destination validation.

For native `select`, pass an exact returned option `value` as `text`. For an
ARIA combobox/listbox, open the control, inspect its returned `options`, and
pass the returned `option_selector`, or a unique exact label as `text`. Only
visible options explicitly owned through the control's descendants,
`aria-controls`, or `aria-owns` can be selected. An empty list can mean a closed
popup or unsupported ownership, rather than a missing choice. Uncommitted
combobox search text is never accepted as a verified selection. A select
postcheck accepts `aria-selected`, or an observed option activation followed by
a matching collapsed display when the popup unmounts. Fill cannot supply that
activation evidence. Public verification defaults to selected-option state.
For a reopened record whose popup is absent, `comparison: "displayed_value"`
compares the displayed value with explicit `displayed_value_only` evidence;
it cannot establish that a search query in an edited form was committed.

`press` with Enter activates links/buttons only. It **never** submits a form from
a text input or combobox. Explicitly click the intended submit button to submit.
Tab, Escape, arrows, and custom keyboard-only behavior require native Computer
controls. `check`/`uncheck` remain idempotent native checkbox operations.

`set_date` requires a native `input[type=date]` and `text` in `YYYY-MM-DD` format.
A detached input validates the date and native min/max/step/required constraints
before the real field is changed. Controls report date format, placeholder,
pattern, input mode, constraints, and validity without exposing field values.
Custom or masked date widgets require their observed format with `fill`, or the
Computer specialist's calendar/native typing. There is no silent native-key or
format guess. Fill uses native setters and input/change events; ordinary fields
also blur to commit validation. ARIA autocomplete fields retain focus so their
options remain available. Subsequent
observations require a matching valid value for 1000ms within a 2200ms budget;
asynchronous resets, validation errors, or navigation prevent verification.
These bounded checks do not prove an application will never reject later.

Interactions separate `applied`, `completed`, `verification`, and
`persistence: "not_verified"`. A click, navigation, or successful setter is not a
saved-data claim. Save using the owner's requested workflow, reopen the stored
record, wait for its expected control, then call:

```json
{"action":"verify","selector":"#saved-field","text":"expected value"}
```

For checkboxes/radios use `checked` instead of `text`. `verify` compares the
current page without returning actual values and marks its scope as
`current_page_only`; the caller must establish that it is the reopened record.
Passwords cannot be compared. Password fills and AutoFill use presence-only
postchecks; local authentication and ambiguous account selection remain with
the owner. Neither a picker selection nor a populated field proves sign-in.

Controls are prioritized by viewport so scrolling changes the useful controls
returned. Respect `controlsTruncated`, `optionsTruncated`, `labelTruncated`, and
`valueOmitted`; never guess an omitted option. Form field values are not returned.

## Latency and boundaries

### Display wake

On macOS, opening an allowed page and executing browser JavaScript refresh an
IOKit user-activity assertion to wake the local display. The OS owns display and
lock state; the helper owns only the assertion ID required for refresh/release.
The assertion follows the user's existing display idle timeout. No background
timer keeps the display lit between tasks. Close/shutdown releases the assertion.
Each native wake failure is returned before the browser operation proceeds.

`browse` with `{"action":"wake"}` requests the same wake without opening a page,
for example before a native Computer handoff. It accepts no other arguments.
`display_wake_requested` means macOS accepted the request, not proof that a
physical monitor is on, the session is unlocked, or a website is signed in.
Read the actual page/window next. A monitor turned off with its hardware button
may not respond to an OS wake request.

This uses Apple's `IOPMAssertionDeclareUserActivity`, which requires no special
privileges. It does not change sleep/password settings, synthesize keystrokes,
unlock the Mac, or bypass Touch ID/MFA. Display sleep and session locking are
separate settings. If the session locks while idle, unattended login-dependent
Safari work will still need the owner. Any change to automatic locking must be
an explicit operator decision outside this tool.

Native verification on a test Mac (wakes its display):

```sh
cargo test --locked --manifest-path tools/zeroclaw-safari-browser/Cargo.toml native_wake_refresh_and_release -- --ignored
```

### Page operations

Link validation checks URL syntax individually, resolves each hostname once per
read, and allows at most eight concurrent resolver futures. Lookups have a
two-second per-host timeout and a four-second page-wide budget. Failed, unsafe,
in-flight or queued destinations remain masked when the budget expires. No
persistent DNS or authorization cache is introduced. System DNS work may finish
after its timed-out future is cancelled. Readiness and mutation verification use
bounded polling instead of fixed page-load
delays. The canonical state remains the live DOM; polling fingerprints are
discarded at the end of each call.

Existing restrictions remain: HTTPS on port 443 without embedded credentials,
public DNS destinations only, and a hard block on LinkedIn/lnkd.in. URL checks
are application guards, not network isolation of Safari or every subresource.
Page content is untrusted and cannot expand the authenticated owner's request.
The caller owns authorization for website actions; this connector does not
authenticate the conversation. No arbitrary script or shell tool is exposed.

Existing macOS automation/Accessibility access and Safari's existing JavaScript
automation setting are prerequisites. This tool does not grant permissions or
change browser security settings. Do not bypass permission or credential prompts.
The connector's fixed native scripts are implementation details; agents use its
MCP interface. Always close the connector window at task end, including failure
or cancellation; failed cleanup retains ownership for retry. Never alter the
owner's other windows/tabs.

## Agent handoff guidance

When a connector capability cannot operate an owner-requested control, the
coordinator can delegate a bounded Safari UI step to an existing specialist
with Computer tools. Pass the exact request, dedicated-window title and URL,
observed limitation and expected result. Keep the window open and stop concurrent
connector actions during the handoff.

The specialist must identify that same window unambiguously using fresh UI
state, operate through documented Computer APIs, verify the visible outcome,
and return control to the coordinator for continuation and cleanup. If identity
is ambiguous, stop the handoff. Do not use another browser or touch unrelated
tabs. This handles missing UI capabilities such as custom widgets, embedded
frames or native keys; it cannot bypass blocked URLs, credential protections,
permission denials or other policy refusals. Main/specialist agent admission and
Computer tools are configured separately; this source import enables neither.

## Build and validation

From the repository root on macOS with Rust and Node.js installed:

```sh
cargo fmt --manifest-path tools/zeroclaw-safari-browser/Cargo.toml -- --check
cargo test --locked --manifest-path tools/zeroclaw-safari-browser/Cargo.toml
node tools/zeroclaw-safari-browser/tests/dom-fixtures.cjs
cargo clippy --locked --manifest-path tools/zeroclaw-safari-browser/Cargo.toml --all-targets -- -D warnings
cargo build --locked --release --manifest-path tools/zeroclaw-safari-browser/Cargo.toml
```

Rust tests exercise bounded readiness/value polling, deadlines, asynchronous
reversion, request validation, AutoFill result interpretation and DNS
policy using injected resolvers. The 160-link/four-host fixture asserts exactly
four lookups; slow-host fixtures verify deadlines and fail-closed results. Node
fixtures execute the exact fixed DOM programs against a minimal offline DOM,
covering asynchronous framework resets, ARIA ownership, selection versus search,
validation, privacy, date metadata, idempotence, selectors and bounded output.
They do not launch Safari or prove behavior on every real website.

For the named real-browser gap, generate the self-contained synthetic fixture:

```sh
node tools/zeroclaw-safari-browser/tests/build-browser-fixture.cjs /tmp/zeroclaw-safari-fixture.html
```

Open that file in Safari using the Computer specialist (not this public-URL-only
connector). It embeds the same fixed production programs, exercises native
constraints and events, delayed reversion, custom widgets, readiness, privacy,
and a synthetic localStorage save followed by a full reload and verification.
The page reports its checks and final PASS/FAIL. A separate manual native-only
date field rejects synthetic events and reports acceptance after trusted native
typing and blur; use it to check the Computer fallback. Begin with no URL fragment when
rerunning it. This proves Safari's DOM behavior, not a particular website's
server persistence or the connector's AppleScript transport. Running initialize/tools-list against the built executable
checks the installed MCP process boundary without opening a browser.

## Local activation and rollback

Back up the installed connector and changed agent guidance privately. Atomically
replace only the executable configured for `safari_browser`, preserving its path
and permissions. Refresh the MCP process through the existing daemon reload
mechanism while no relevant task is active, then verify version/tool discovery
and runtime health. Update the local coordinator/specialist guidance from the
handoff contract above. A core-runtime rebuild is unnecessary for this standalone
connector. Do not rerun broad installers or modify unrelated services/state.

Rollback restores the backed-up executable and only the affected guidance,
then reloads the connector and verifies health. Preserve newer credentials,
delivery receipts, schedules, signing setup and unrelated edits.

## Web components and page visibility

Version 0.1.6 walks nested **open** shadow roots when reading controls and page
text. It follows assigned slot content, keeps control IDs scoped to their root,
and returns `shadow:["host selector","nested host selector","control selector"]`
paths for shadow controls. Copy these paths exactly into interaction,
verification, AutoFill, or readiness requests. Ordinary CSS interaction selectors
retain their document scope. A plain CSS readiness query searches open roots and
must match one control; ambiguous matches now return an explicit DOM error.

Form input/change events cross shadow boundaries. Hidden, inert and disabled
composed ancestors remain blocked, and page text excludes input values and script
content. Closed roots, cross-origin frames and browser-native UI still require
Computer; missing controls are not proof that no controls exist. Scans are limited
to 50,000 elements and 256 open roots; paths are limited to 16 segments and 1,024
UTF-8 bytes. Exceeding a limit reports an error rather than selecting a partial path.

A hidden Safari document is not ready. `document_hidden` in readiness or
verification means the Mac must be unlocked and the dedicated Safari window made
visible before interaction. The connector will not silently treat a background or
locked-screen page shell as a loaded calendar. `custom_elements_pending` means
custom elements have not yet registered; a timed-out read remains incomplete.
The page's `rendering` metadata reports visibility and open/pending component
counts, derived from the current document. Nothing is persisted between calls.

The shadow regression fixture uses the exact production DOM programs:

```sh
node tools/zeroclaw-safari-browser/tests/build-browser-fixture.cjs /tmp/zeroclaw-shadow-fixture.html shadow
```

Open it in the browser. Its PASS result covers nested roots, duplicate IDs,
slotted text, composed form events, verification, hidden/disabled/inert hosts,
closed roots, stale paths, delayed component registration and document visibility.
No real appointment or external submission is made by this fixture.
