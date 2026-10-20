# Development

## Building

Building requires Rust 1.88 or newer with the MSVC toolchain (Visual Studio C++
build tools and a Windows SDK).

```powershell
git clone https://github.com/Sofahamster/alphashot
cd alphashot
cargo build --release --locked
```

## Checks

```powershell
cargo test --locked
cargo fmt -- --check
cargo clippy --locked --all-targets -- -D warnings
```

CI runs these on `windows-latest` and checks that the crate builds with
Rust 1.88.

## Desktop tests

The ignored tests need an idle interactive desktop. They show, animate and move
disposable windows, then restore the cursor and the previous foreground window.
Input from you or another application can make the foreground and menu tests
fail.

```powershell
# Disposable windows on the desktop.
cargo test --locked --test windows_capture -- --ignored --nocapture

# Set WebP and include the cursor, apply, choose code.exe, then cancel the second picker.
cargo test --locked --bin alphashot interactive_picker_restores_terminal -- --ignored --nocapture

# Covered-window thumbnail correctness and capture/resize/encode timings (writes no files).
cargo test --locked --release --test windows_capture snapshots_occluded_windows_without_changing_focus_or_placement -- --ignored --nocapture
```

Unit tests cover alpha reconstruction, animated client pixels, calibrated
shadows, filenames, process names, fuzzy matching, pick-mode switching, the
settings editor, output formats, DPI, prerequisite errors and overwrite
protection. The desktop tests cover focused and unfocused capture, menus wider
than their parent, screen-edge repositioning, maximized windows, focus
restoration across processes, CLI output and cancellation.

Thumbnail tests cover debouncing, discarding stale results, key handling during
a blocked capture, cache limits and eviction, capture failure, reuse on resize,
and hiding and restoring images around the settings editor. The Windows Graphics
Capture test checks the pixels of a covered window and confirms that its
placement and the foreground window stay unchanged.

## Thumbnail timings

Five release-build samples of a covered 1266×893 test window, reduced to
a 280×197 thumbnail, on the author's desktop:

| Stage | Time |
| --- | --- |
| WGC/D3D initialization (once per picker) | 261 ms |
| Capture and GPU readback | 56–221 ms |
| Cache downsize and thumbnail resize | 8.4–9.2 ms |
| Sixel encoding | 2.10–2.20 ms |

The test window has large flat color areas, and busier applications take longer to
encode. The numbers measure CPU and API work. Add the 100 ms debounce, any
running job, input polling, output transmission and the terminal's own rendering
to get the delay before an image appears.

## Library

The crate exposes capture, output and alpha recovery as separate modules. The
alpha API expects white and black composites of the same size; the CLI has no
offline recovery command.
