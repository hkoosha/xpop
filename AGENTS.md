# Agent instructions for xpop

## Project and working style

xpop is a Rust 2024 X11 pull-down application host. It embeds an external
application and toggles visibility over D-Bus. The production implementation
is in `src/main.rs`, using `x11rb`/XCB, `dbus`, and small native Linux helpers.

- Keep changes focused on the requested behavior. Prefer straightforward,
  maintainable code over new abstractions, compatibility layers, or speculative
  features. Do not special-case xterm or WezTerm to hide a general embedding bug.
- Honor explicit file restrictions literally. A documentation-only or named-file
  request does not authorize changes to other code, manifests, lockfiles,
  generated files, or formatting. Do not resume unrelated work during it.
- Treat user-reported failures as evidence, not something to dismiss or repeatedly
  ask the user to prove. Inspect the existing implementation before proposing a
  cause; distinguish observed behavior from inference.
- Preserve unexpected repository changes; they may be the user's work. Do not
  revert, overwrite, or remove unrelated changes.
- Follow existing code conventions and `rustfmt.toml`: 80-column formatting,
  vertical imports and parameters, and the established brace layout. Explicit
  `return` statements are intentional; `clippy::needless_return` is allowed.
- Avoid unnecessary allocation, repeated X11 round trips, and polling work in
  the event loop. Correctness comes first, but a fix that creates a hot loop is
  not a fix.

## Development environment: no display testing

This development server has no running Xorg server or display. Do not attempt
live X11 tests, launch graphical clients, or start/install Xorg, Xvfb, or Xephyr
unless the user explicitly changes this constraint.

Use display-free verification instead. In-memory protocol/event models and
native Linux process/syscall checks are appropriate. They do not prove actual
rendering, compositor behavior, or window-manager integration. State that limit
rather than claiming a visual result.

Keep the reusable harness in the project-root `test-harness/` directory. It is a
separate Cargo package. Its build script extracts current production code from
`src/main.rs`; do not replace that with frozen copies of implementation logic or
leave the only useful verification in a temporary directory.

## Runtime invariants

### Launch, discovery, and event processing

- Process startup, PID properties, window creation, mapping, and embedding are
  asynchronous. An early lookup miss must not strand a black/empty host until
  the user toggles it again.
- Preserve deadline-based discovery with bounded backoff. Do not retry a full
  window-tree scan on every X11/D-Bus wakeup or let unrelated activity postpone
  discovery indefinitely.
- Drain buffered XCB events before blocking on file descriptors. Synchronous
  replies and checked requests can consume socket data while leaving events
  queued internally; `POLLIN` alone is not sufficient.
- Keep visibility, client readiness, and pending focus intent distinct. Account
  for stale map, unmap, reparent, and focus notifications. Generated events must
  converge, not trigger endless remapping, reparenting, or focus handoffs.

### Focus and abrupt termination

- Keyboard input must remain usable if the embedded client, xpop, or both die,
  including `SIGKILL`. Do not rely on an exit handler or xpop surviving to restore
  focus.
- Use X-server-side `InputFocus::POINTER_ROOT` reversion for host and embedded
  focus requests. `RevertToParent` changes the next reversion to `RevertToNone`:
  client destruction can focus the host, then host destruction can cause X to
  discard keyboard input.
- An already-focused fast path must also check the reversion policy, not merely
  the focused window ID.
- Do not continuously force focus or steal it from another application. Ignore
  stale/ancestor/grab notifications that do not represent a real host focus
  handoff. Clear pending focus only after a successful or satisfied request.

### Transparency

- The host's own background must be fully transparent. Let the embedded
  application determine its background opacity and rendering.
- Select a supported 32-bit TrueColor visual on the host's actual X11 screen,
  confirmed by a matching Render picture format with a nonzero alpha mask.
  Depth 32 alone does not establish alpha support; a pixmap format alone does
  not establish a usable window visual.
- Negotiate Render before querying formats. Use a matching colormap and explicit
  zero background and border pixels. Do not inherit an opaque root visual or a
  different-depth parent's border pixmap.
- Keep the colormap alive while the host uses it and release it on teardown.
  Unsupported alpha visuals must produce an explicit error, not a silent opaque
  fallback.
- Do not implement this through whole-window opacity: that would also fade the
  embedded text/content. Visual transparency does not imply click-through input.
- Actual desktop transparency still requires a working compositor. Do not claim
  that display-free tests establish the visible result.

## Native safety and process ownership

- Document the safety contract of each repository-owned `unsafe` block. Check
  fallible native calls and preserve their OS errors instead of treating failure
  values as valid results.
- Validate process/group selectors before signalling. Avoid accidentally using
  selectors such as zero or negative values that target unintended processes.
- Retain ownership through `std::process::Child` and use `Child::try_wait` for
  nonblocking child status checks rather than unchecked raw wait results.
- Preserve `EINTR` so the event loop can reap children. Report invalid poll
  descriptors rather than interpreting them as ordinary readable activity.
- Keep `pre_exec` hooks async-signal-safe: no allocation, locks, or logging.
  Capture the expected parent before spawning, check parent-death setup, and use
  correctly sized variadic arguments. Hook failures must propagate via `spawn`.

## Verification and delivery

For code changes, use the applicable commands below when the requested scope
permits their generated artifacts. `--offline` assumes dependencies are cached.

```sh
cargo fmt --check
cargo fmt --manifest-path test-harness/Cargo.toml --check
cargo check --offline
cargo test --offline --manifest-path test-harness/Cargo.toml
cargo run --offline -- --help
```

- Root-package checks do not replace running the separate harness.
- Prefer regression coverage for observable behavior: event ordering, lifecycle
  transitions, focus ownership, visual selection boundaries, and error paths.
  Avoid assertions that merely pin source text, incidental wording, or mock calls.
- Reproduce bugs in a display-free scenario where possible, then verify the fix.
  Exercise changed behavior rather than relying on compilation or CLI help alone.
- Keep useful regression checks reusable. Remove temporary scaffolding only when
  the current scope permits it. Update existing documentation when behavior
  changes, without creating unrelated documents.
- Report the concrete change, verification actually run, and remaining limits.
  Do not present a model, passing build, or untested hypothesis as live Xorg proof.
  Do not claim completion for an unverified rendering result.

Protocol references:
- [XSetInputFocus and reversion semantics](https://www.x.org/archive/current/doc/man/man3/XSetInputFocus.3.xhtml)
- [X Render formats and initialization](https://www.x.org/releases/X11R7.7/doc/renderproto/renderproto.txt)
