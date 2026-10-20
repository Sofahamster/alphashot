# Usage

## Window picker

Run `alphashot` to open the picker. Type to fuzzy-filter process names and
window titles; `ntpd` matches `notepad.exe`, and search words may appear in
either order. The picker highlights matching letters and shows a scrollbar for
long lists. Move the selection with Up/Down, Page Up/Page Down, Home/End or the
mouse, then press Enter or double-click a row to capture.

Wide terminals show the selected window's title, process, handle, size and DPI
beside the list, along with the current capture settings and clickable
pick-mode buttons. The layout grows with the terminal; small terminals get a
stacked layout. The picker needs at least 60 columns and 12 rows. It uses a
fixed dark palette, and `NO_COLOR` reduces it to inverse selection highlighting.

- **F2** switches to click selection: hover to highlight a window, then click it.
- **F3** picks the window under the cursor after a delay (two seconds by default).
- **F4** or **Tab** opens capture settings; clicking a settings row also opens it.
- **F5** refreshes the window list and the thumbnails; **Ctrl+U** clears the search.
- **Esc** or **Ctrl+C** cancels. Alphashot restores the terminal state on exit.

Processes alphashot cannot inspect show up as `<unknown>`, with their titles and
PIDs intact.

## Thumbnails

The side panel shows an opaque snapshot of the selected window once it has room
for three image rows. Alphashot takes these snapshots with
[Windows Graphics Capture](https://learn.microsoft.com/en-us/windows/win32/api/windows.graphics.capture.interop/nf-windows-graphics-capture-interop-igraphicscaptureiteminterop-createforwindow)
(Windows 10 1903 or newer). It captures a window behind the terminal without
activating, moving or resizing it. Thumbnails skip the backdrops, so they show
no transparency or shadow.

Alphashot waits 100 ms after a selection change, then captures, resizes and
encodes the thumbnail on one background worker. It runs at most one job and
keeps one replaceable pending request, and it discards results for a window you
have already left. It keeps four recent snapshots (up to 1024×768 each) and two
thumbnail sizes per snapshot in memory and writes no preview files. Returning to
a cached window reuses its snapshot; press F5 to take fresh ones.

Minimized, closed, restricted or unsupported windows get a placeholder, and so
does a window that changes size during the snapshot. Protected content can come
out blank. Windows may draw a brief
[capture border](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture.graphicscapturesession.isborderrequired);
alphashot does not request borderless-capture consent.

Thumbnails need a Sixel-capable terminal such as Windows Terminal 1.22 or newer.
`--no-preview`, `NO_COLOR`, `TERM=dumb` and a redirected stdin or stdout turn
graphics off. A terminal without Sixel support ignores the image, and the text
list keeps working. The settings editor hides an overlapping thumbnail and
restores it from memory when it closes.

## Capture settings

The settings editor changes the output format, the focused or unfocused look,
cursor inclusion, the alpha mode and the display scale. Select a setting with
Up/Down or Tab and change it with Left/Right or Space, or click the arrows or
the value. **Enter** applies the changes and returns to the list; **Esc**
discards them.

Settings start from your command-line options and last for the current run;
alphashot does not save them. Changing the format updates the extension of an
explicit output filename, and existing files still require `--force`.

The scale setting offers the window's own DPI, automatic high DPI and each
active display above 100% scale. Without such a display, it lists the
[setup steps](high-dpi.md). F5 refreshes the display list inside the editor.

## Command-line examples

```powershell
# Open the picker. There are no subcommands.
alphashot

# A unique title match captures without the picker.
alphashot --window-title "Notepad" -o notepad.png
alphashot --window-hwnd 0x123456 -o window.webp

# Start in a pick mode, for scripts and shortcuts.
alphashot --pick-mode cursor --pick-delay 3
alphashot --pick-mode overlay

# Capture the inactive look and include the cursor.
alphashot --window-title "Notepad" --focus-state unfocused --include-cursor

# Wait after selection so you can open a menu.
alphashot --pick-mode overlay --post-pick-delay 4

# Render on a display above 100% scale (see high-dpi.md).
alphashot --window-title "Notepad" --high-dpi

# Save the white and black composites next to the final image.
alphashot --window-title "Notepad" -o window.png --raw-dir .\raw
```

If several windows match `--window-title`, the picker opens in a terminal. In
unattended use, alphashot prints the candidates and exits with an error; pass a
more specific title or a handle. It does not pick the first match for you. The
candidate list uses the `--list-windows` format.

`--list-windows` prints the selectable windows to stdout and exits without
capturing; `--window-title` keeps only windows whose title contains the text,
ignoring case. `--list-displays` prints the active displays and exits. Both
print one header line, then one tab-separated row per item, with control
characters in text fields escaped (a tab in a title appears as `\t`).

```text
--list-windows   hwnd  pid  process  width  height  dpi  title
--list-displays  name  left  top  width  height  work_width  work_height  dpi  scale  description
```

`hwnd` is `0x` plus uppercase hex, `process` is `<unknown>` when Windows denies
access, and `scale` is a percentage. Pass a `hwnd` to `--window-hwnd`, or a
`name` to `--high-dpi-display`.

Capture defaults: focused appearance, automatic shadow geometry, cropping to
visible alpha, no cursor, and restoring the previous foreground window
afterwards. For each backdrop, alphashot takes screenshots frame by frame until
two consecutive frames match outside the client area, for at most
`--settle-ms` (250 ms by default). A window that has stopped changing ends the
wait after two frames; one that is still changing at the limit triggers a
warning. `--settle-ms 0` takes the first frame that shows the backdrop. After
activating a window that was not in front, alphashot waits a fixed 80 ms for
activation animations, because some of them pause long enough to look stable.

## Output files

Alphashot writes PNG, lossless WebP or TIFF (LZW) with straight alpha. PNG and
TIFF carry the capture DPI; WebP files from alphashot have no DPI metadata. Rust
libraries handle all encoding, and lossy WebP is not available.

Without `-o`, alphashot saves `YYYY-MM-DD-HH-MM-SS-window-title.png` in the
current directory, using local time and a shortened slug of the title.
`--out-format webp` or `tiff` changes the default extension.

Alphashot writes each file atomically and replaces an existing file only with
`--force`. Automatic names get the same protection. Raw composites default to
`capture-white.png` and `capture-black.png`; change the prefix with
`--raw-prefix`. The final image path must differ from both raw paths,
even with `--force`.

## Terminal output and exit codes

An interactive capture ends with a summary of the window title, size, format
and DPI. The filename is an OSC 8 hyperlink; Ctrl+click opens it in Windows
Terminal. A Sixel thumbnail of the result follows on a white background, and the
saved file keeps its full resolution and alpha. `--no-preview` turns the
thumbnail off. If the thumbnail fails, alphashot prints a warning; the file is
already saved at that point.

File links keep Unicode characters literal to avoid a
[percent-decoding issue](https://github.com/microsoft/terminal/issues/20803) in
Windows Terminal and ShellExecute. Spaces and reserved URI characters get
escaped.

With stdin or stdout redirected, stdout carries the saved paths, one per line,
and progress and errors go to stderr. Exit codes: 0 for success, 1 for runtime
errors or cancellation, 2 for invalid arguments.
