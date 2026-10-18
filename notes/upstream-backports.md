# Fixes taken early from Ghostty's main

Pepper is based on Ghostty 1.3.1. These fixes come from Ghostty's later code on `main`, taken before a release has them. Drop each one when Pepper moves onto a Ghostty release that has it (`git merge-base --is-ancestor <sha> v<X.Y.Z>`). They are applied as Ghostty wrote them, tests included, except where a line says how it was adapted: mostly for Zig 0.15.2 (no `std.Io`, so tests call `Terminal.init(alloc, …)` and `try` the VT stream) and for code 1.3.1 doesn't have yet.

Taken 2026-10-18, from `upstream/main` at `d2865b8f5`.

## Security

- `5dc28bb8e` (#14509): narrowing the screen without reflow could split a wide character, and a later insert (ICH) on that row then wrote one cell past its end. Test adapted (`resize(alloc, cols, rows)`).
- `a41248013` (#14262): tmux control mode, the list-windows action pointed at a list freed on return (use-after-free).
- `1a93d5395` (#14680): tmux control mode, a pane's scroll region was applied without checking it fits the screen. Test adapted: 1.3.1's list-panes format has one more field (`mouse_any_flag`), and 1.3.1 sets the cursor on the primary screen without switching to it.
- `e5840bb9b` (#13630): Kitty graphics, a placement with `r=4294967295` moved the cursor about 4 billion times under the terminal lock, and its geometry overflowed u32. The C API part (`src/terminal/c/kitty_graphics.zig`) is left out: 1.3.1 has no C API for Kitty graphics. `pixelSize` is `calculatedSize` in 1.3.1.
- `ae0ff51c4` (#13648): macOS, a second OSC 52 read while the confirmation sheet was open freed the request inside its own callback.
- `7c49e723a` (#13997): ED 22 (clear the screen into scrollback) moved the cursor to another page without moving its style and hyperlink references. Adapted: `&node.data` for `node.page()`, `.capacity` for `capacity()`, `max_scrollback` for `max_scrollback_bytes`.
- `bd21ff153` (#13631): VS15 after a wide character under the cursor made the cursor position underflow.
- `cca4c788a` (#11965): search kept pointers into pages that cmd+K or CSI 3J had freed (use-after-free).
- `0509f00ad` + `83027407e` (#12349): no size limit on a Kitty graphics APC payload (now 65 MiB), and a leak on an invalid command. `apc.zig` ported by hand.
- `cc53940b4` (#14687): paste protection let a carriage return through, which runs a command like a newline. Only `isSafe` (1.3.1 has no `isSafeWith` or `ghostty_terminal_paste`), its tests, and the doc comments in `Config.zig` and `paste.h`. Bracketed pastes are unchanged (`clipboard-paste-bracketed-safe`).
- `4803d58bb` (#12025): `ghostty_surface_free_text` took one argument in Zig and two in the header, so Zig read the surface as the text: it could free memory it didn't own, and the text leaked.
- `e9ca0f8c9` (#12463): a click on a link read the terminal without the renderer's lock.
- `1eecfe089` (#13364): macOS, a closed surface was freed later by a detached task, after its userdata was gone (use-after-free).
- `6246c288a` (#12894): `setSelection` compared the new selection with the old one after freeing the old one's pins.
- `d0659ba52` (#13636): macOS, a new tab while windows were in native fullscreen could crash in AppKit.
- `b60970ce2` (#13642): a font without a CoreText display name crashed.
- `54fe8e188` + `012548367` (#13634, #14683): cmd+click on a link (OSC 8, or text matched by `link`) went straight to Launch Services, so terminal output could open a `.command`, `.app` or `.terminal` file, a custom scheme, or a target hidden by invisible characters. Swift only: `UntrustedURL.swift`, `UntrustedURLAlert.swift` and their tests as in Ghostty's `main` (the block alert says "Pepper Blocked This Link"); `openURL` in `Ghostty.App.swift` sends every `unknown` link through them, since 1.3.1 sends both kinds as `unknown`, and checks plain paths as file URLs. `UntrustedURLAlert.swift` is kept out of the iOS target in the Xcode project. Left out: the `osc8` kind and `link-osc8` config in the core, the hover preview's escaping, and `os/open.zig` refusing untrusted kinds (the macOS app always handles the action).

## Performance

- `e69dc2bee` (#13585): the renderer's counter for freeing its terminal state was never reset, so after about 12 minutes it rebuilt the state every frame. In `generic.zig`, next to Pepper's custom-shader change.
- `a60cd15bb` (#13490): a Metal buffer that grew allocated the size of an item times more than it meant to (the doubled size was already in bytes).
- `7cb44fea3` (#13258): the renderer built a string of the whole screen every frame to look for links even when none could be highlighted. Test adapted (`try`).
- `14d9e600a` (#12760): the renderer rebuilt its cells for hidden surfaces (other tabs, minimized windows); it now catches up when the surface is shown again. Only this commit of the PR; the rest is GTK.
