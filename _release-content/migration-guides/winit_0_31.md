---
title: Winit 0.31 beta
pull_requests: []
---

Bevy's window backend now uses Winit 0.31.0-beta.4. Bevy window and input
messages keep their existing types. If you depend on Winit directly for a
custom integration, use the same Winit version and migrate your raw Winit
event matching.

- Winit windows and active event loops are traits; stored Winit windows are
  now owned trait objects. Use `as_ref()` on a boxed window to borrow its trait.
- Use Bevy's `EventLoopProxyWrapper::send_event` for `WinitUserEvent` payloads;
  Winit's low-level `EventLoopProxy` now sends wakeups, not payloads.
- Winit's raw window messages now use `SurfaceResized` and `Pointer*` events.
  Winit cursor types have moved from `winit::window` to `winit::cursor`.
