<p align="center">
  <img src="assets/logo.svg" width="112" alt="">
</p>

<h1 align="center">alphashot</h1>

<p align="center">
  Window screenshots for Windows 10 and 11 with a transparent background, rounded corners and the DWM shadow.
</p>

<p align="center">
  <img src="assets/picker.png" alt="The alphashot picker in Windows Terminal, captured by alphashot with rounded corners and shadow on a transparent background">
</p>

Alphashot captures a single window over a white and a black backdrop and solves
for per-pixel alpha. The output has straight alpha: the frame corners are cut
out and the shadow is semi-transparent, so the image composites onto any
background. The screenshot above shows alphashot capturing its own picker.

## Install

```powershell
cargo install alphashot --locked
```

Building requires Rust 1.88 or newer with the MSVC toolchain (Visual Studio C++
build tools and a Windows SDK). Capturing requires Windows 10 1903 or later and
an interactive desktop.

Prebuilt x86_64 binaries are on the
[releases page](https://github.com/Sofahamster/alphashot/releases). The
executable is unsigned, so SmartScreen may warn on first launch.

## Usage

Run `alphashot` from a terminal. The picker lists top-level windows; type to
fuzzy-filter by process name or title (`ntpd` matches `notepad.exe`) and press
Enter. The backdrops flash behind the window for a second or two, so keep the
window still until the capture finishes. Alphashot writes
`YYYY-MM-DD-HH-MM-SS-<title>.png` to the current directory and refuses to
replace an existing file without `--force`.

| Key | Action |
| --- | --- |
| Enter, double-click | Capture the selected window |
| F2 | Pick by clicking a window |
| F3 | Pick the window under the cursor after `--pick-delay` seconds (default 2) |
| F4, Tab | Capture settings: format, focus state, cursor, alpha mode, scale |
| F5 | Refresh windows and thumbnails |
| Ctrl+U | Clear the search |
| Esc, Ctrl+C | Quit |

In Windows Terminal 1.22 or newer, or another terminal with Sixel support, the
picker shows a thumbnail of the selected window, and the summary after a capture
shows the result.

```powershell
# Capture by title or handle without the picker.
alphashot --window-title "Notepad" -o notepad.png
alphashot --window-hwnd 0x123456 -o window.webp

# Click a window, then open a menu within four seconds.
alphashot --pick-mode overlay --post-pick-delay 4

# Unfocused appearance with the mouse cursor.
alphashot --window-title "Notepad" --focus-state unfocused --include-cursor

# Render on a display scaled above 100% (see docs/high-dpi.md).
alphashot --window-title "Notepad" --high-dpi
```

Output formats are PNG, lossless WebP and TIFF. With stdout redirected,
alphashot prints the saved paths and nothing else; exit codes are 0 for
success, 1 for errors or cancellation and 2 for invalid arguments.
`alphashot --list-windows` prints window handles as tab-separated values for
scripts, and `--window-hwnd` then captures without the picker.
`--list-displays` lists display names for `--high-dpi-display`. See
`alphashot --help` for all options.

## Limitations

The client area comes from one frame and stays opaque, which keeps blinking
carets and animations from punching holes into the image.
`--transparent-client` recovers translucency inside the window for static
content. Acrylic and Mica blur depend on what lies behind the window, and
alphashot cannot reconstruct them. Video and DRM-protected content can come out
black.

Alphashot moves a window near a screen edge inward so its shadow fits, then
restores its position. It skips the move while a menu is open, so position the
window before opening the menu.
Capturing an elevated window requires running alphashot elevated as well.

## Documentation

- [Usage](docs/usage.md): picker, thumbnails, settings and output files
- [How it works](docs/how-it-works.md): alpha recovery, shadow padding, menus and elevation
- [High-DPI capture](docs/high-dpi.md): virtual display setup for 200% captures
- [Development](docs/development.md): building from source, tests and CI

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or
[MIT license](LICENSE-MIT) at your option.

Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this crate by you, as defined in the Apache-2.0 license, shall
be dual licensed as above, without any additional terms or conditions.
