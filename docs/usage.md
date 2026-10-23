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

The side panel shows a snapshot of the selected window once it has room
for three image rows. Alphashot takes these snapshots with
[Windows Graphics Capture](https://learn.microsoft.com/en-us/windows/win32/api/windows.graphics.capture.interop/nf-windows-graphics-capture-interop-igraphicscaptureiteminterop-createforwindow)
(Windows 10 2004 / build 19041 or newer). It captures a window behind the terminal
without activating, moving or resizing it. Native alpha is preserved in the
snapshot and composited over white for presentation. WGC omits the outer DWM
shadow but can retain shadow inside rounded frame corners.

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
cursor inclusion, the alpha mode, the display scale and the selected preset.
The Backend and Shadow rows choose the capture source and shadow inclusion;
TIFF alpha chooses alpha association when saving TIFF.
Select a setting with Up/Down or Tab and change it with Left/Right or Space,
or click the arrows or the value. **Enter** applies the changes and returns
to the list; **Esc** discards them.
In a short terminal, Up/Down, Tab or the mouse wheel scrolls the settings;
the panel title shows which rows are visible. Apply, Cancel and errors remain
visible while scrolling.

Settings start from your command-line options and last for the current run.
The Preset row cycles saved presets and shows the current selection; applying
a preset keeps explicit command-line values. Loading a preset reports errors
in the editor and leaves the settings unchanged on failure. Changing the format
updates the extension of an explicit output filename, and existing files still
require `--force`.

The scale setting offers the window's own DPI, automatic high DPI and each
active display above 100% scale. Without such a display, it lists the
[setup steps](high-dpi.md). F5 refreshes the display list inside the editor.

## Capture backend and shadows

`--backend backdrop` is the default: alphashot places white and black backdrops
behind the window and recovers transparency from the pair. This backend can
request a focused or unfocused appearance with `--focus-state`.

`--backend wgc` uses Windows Graphics Capture without activating the target.
It preserves the window's native alpha and current appearance; `--focus-state`
does not change that appearance. The picker's Look row shows **Current (WGC)**
and explains why focus changes require the Backdrop backend. Its Alpha row
shows **Native alpha (WGC)**: `--transparent-client` and backdrop alpha recovery
do not change WGC's native alpha. Mica and Acrylic come out as Windows currently
composes them, Windows may show its capture border while capturing, and windows
that exclude themselves from screen capture fail. `--raw-dir` requires the
backdrop backend because WGC has no white/black composite pair.

Ordinary WGC captures leave position and size unchanged. `--client-size`
temporarily resizes to exact physical client dimensions. Explicit `--high-dpi`
moves to a higher-DPI display only when needed, without activating the window,
and restores its placement afterwards. Automatic selection chooses the highest
DPI, then the largest display area; it skips relocation when the window already
uses that DPI. `--high-dpi-display` requests a particular monitor. Geometry
changes require a normal, unsnapped window. See [high-DPI setup](high-dpi.md).

`--no-shadow` crops to the window border, keeping the title bar and client
translucency. Standard native frames use an approximate corner mask with a
one-pixel feather: square, 4-DIP or 8-DIP radii, scaled by the target's DPI.
This aims for a smooth appearance; the antialiased boundary can differ slightly
from Windows' exact rendering. Custom full-client chrome, application-defined
regions and unidentified frames fail clearly. Qualifying per-pixel layered
sources keep their existing alpha without a geometric mask.

The backdrop default includes the DWM shadow. WGC omits the outer shadow but
can retain shadow in rounded corners; its Shadow row distinguishes **Outer
shadow omitted** from **Excluded (soft corners)**. Changing padding or setting
`--pad 0` alone does not remove a shadow. No-shadow captures stay tight even
with `--no-crop` and the default padding.

Both choices are available in F4 settings and saved presets, and `--repeat`
retains them for each capture. Use `--no-shadow=false` to override a preset's
enabled setting; WGC still does not add an outer shadow in that mode.

```powershell
alphashot --window-title "Notepad" --backend wgc
# Maximize Notepad before requesting complete shadow removal.
alphashot --window-title "Notepad" --backend backdrop --no-shadow
```

## Saved presets

Save a named set of defaults with `--save-preset NAME`; this writes the preset
and exits without capturing. Apply it with `--preset NAME`, or select it in
the picker's F4 settings. `--list-presets` prints names in alphabetical order.

```powershell
alphashot --save-preset docs --out-dir .\screenshots --out-format webp --include-cursor --margin 8
alphashot --preset docs --window-title "Notepad"
alphashot --preset docs --include-cursor=false --high-dpi=false
alphashot --list-presets
```

Presets store only the output folder and format, TIFF alpha association,
cursor inclusion, focus state, backend, shadow exclusion, alpha margin, shadow
padding and padding mode, and high-DPI selection. Explicit command-line values
override each preset field, including values equal to the normal defaults.
Use `--include-cursor=false`, `--no-shadow=false` or `--high-dpi=false` to
disable a preset's enabled setting; the latter also clears its inherited
display name.
An explicit `--high-dpi-display` can use `high-dpi=true` from a preset.
`--out FILE` always takes precedence over the saved folder. `--clipboard-only`
ignores that inherited folder and TIFF alpha mode and creates no capture files.

The default file is `%APPDATA%\alphashot\presets.ini`; `--presets-file PATH`
chooses another file. Reading a missing file creates nothing. Saving creates
the parent directory if needed and atomically replaces the file, preserving
other presets. Saving an existing name replaces its settings. Names are case
sensitive and contain 1–64 ASCII letters, digits, underscores or hyphens.

The file is UTF-8 with one `[name]` section per preset. Blank lines and lines
starting with `#` or `;` are ignored. Keys and values may have surrounding
whitespace; values are literal, without quotes, escapes or inline comments.
Omitted keys use normal capture defaults. An empty folder or display means
none. Unknown or duplicate keys, duplicate sections, invalid values and a
display without `high-dpi=true` are errors. Example:

```ini
[docs]
out-dir=C:\Screenshots
out-format=webp
tiff-alpha=straight
include-cursor=true
focus-state=focused
backend=backdrop
no-shadow=false
margin=8
pad-mode=shadow
pad=4
high-dpi=false
high-dpi-display=
```

## Command-line examples

```powershell
# Open the picker. There are no subcommands.
alphashot

# A unique title match captures without the picker.
alphashot --window-title "Notepad" -o notepad.png
alphashot --window-hwnd 0x123456 -o window.webp

# Combine selectors and wait up to ten seconds for the target to appear.
alphashot --process notepad.exe --window-class Notepad --window-title "notes" --wait 10

# Select once, then capture each change with Enter.
alphashot --repeat --window-title "Notepad" -o notes.png

# Start in a pick mode, for scripts and shortcuts.
alphashot --pick-mode cursor --pick-delay 3
alphashot --pick-mode overlay

# Capture the inactive look and include the cursor.
alphashot --window-title "Notepad" --focus-state unfocused --include-cursor

# Wait after selection so you can open a menu.
alphashot --pick-mode overlay --post-pick-delay 4

# Render on a display above 100% scale (see high-dpi.md).
alphashot --window-title "Notepad" --high-dpi

# Request exact physical client pixels, plus the frame and shadow.
alphashot --window-title "Notepad" --high-dpi --client-size 1200x800

# Save the white and black composites next to the final image.
alphashot --window-title "Notepad" -o window.png --raw-dir .\raw
```

Window selectors combine with AND and ignore case: `--window-title` matches a
title substring, `--process` matches an exact executable basename including its
extension, and `--window-class` matches an exact Win32 class. Process and class
selectors also find untitled windows. An inaccessible process name cannot match
`--process`. A unique match captures directly in list mode; several matches
open the picker in a terminal. In unattended use, alphashot prints the
candidates and exits with an error; narrow the selectors or pass a handle. It
does not pick the first match for you. The candidate list uses the
`--list-windows` format.

`--wait SECONDS` polls for at least one matching visible window, for up to 0–3600
seconds; Ctrl+C cancels it. It requires at least one selector, and zero checks
once without waiting. A timeout exits with an error. Once matches appear,
normal selection rules apply, including ambiguity errors. All selectors apply
to listing, the picker's F5 refresh, cursor sampling and overlay hover. Waiting
runs before the selected pick mode starts. `--window-hwnd` conflicts with
selectors and `--wait`.

`--list-windows` prints the selectable windows to stdout and exits without
capturing; it accepts all window selectors and `--wait`. Without `--wait`, no
matches produces just the header. `--list-displays` prints the active displays
and exits. Both print one header line, then one tab-separated row per item, with control
characters in text fields escaped (a tab in a title appears as `\t`).

```text
--list-windows   hwnd  pid  process  width  height  dpi  title  class
--list-displays  name  left  top  width  height  work_width  work_height  dpi  scale  description
```

`hwnd` is `0x` plus uppercase hex, `process` is `<unknown>` when Windows denies
access, and `scale` is a percentage. Pass a `hwnd` to `--window-hwnd`, or a
`name` to `--high-dpi-display`. The final `class` column is the value for
`--window-class`.

Backdrop capture defaults: focused appearance, automatic shadow geometry, cropping to
visible alpha, no cursor, and restoring the previous foreground window
afterwards. For each backdrop, alphashot takes screenshots frame by frame until
two consecutive frames match outside the client area, for at most
`--settle-ms` (250 ms by default). A window that has stopped changing ends the
wait after two frames; one that is still changing at the limit triggers a
warning. `--settle-ms 0` takes the first frame that shows the backdrop. After
activating a window that was not in front, alphashot waits a fixed 80 ms for
activation animations, because some of them pause long enough to look stable.

With the backdrop backend, `--client-size WIDTHxHEIGHT` temporarily sizes the client area in physical
pixels on the capture display, after any high-DPI move. Dimensions must be
positive and contain at most 40 million pixels; the full capture must also fit
the monitor workspace. Frame and shadow pixels are additional. The application
renders at the monitor's actual DPI; this option does not resample or upscale
the image. Application size limits produce an error with requested and actual
dimensions. Alphashot restores the original window placement and geometry on
success, cancellation or failure. Close open native menus before resizing.

## Output files

Alphashot writes PNG, lossless WebP or TIFF (LZW), using straight alpha by
default. PNG and TIFF carry the capture DPI; WebP files from alphashot have no
DPI metadata. Rust libraries handle all encoding, and lossy WebP is not available.

PNG and WebP require straight alpha. TIFF also supports declared associated
alpha: `--tiff-alpha premultiplied` multiplies only the encoded TIFF color
samples by alpha and writes `ExtraSamples=AssociatedAlpha`; the default
`--tiff-alpha straight` writes `UnassociatedAlpha` and preserves the samples.
The input image, previews and clipboard PNG remain straight. Multiplication
rounds to 8-bit samples, losing precision at low alpha and discarding color
under fully transparent pixels. TIFF compression remains lossless, but this
color conversion is not reversible.

Premultiplied mode requires effective TIFF output, including `--out image.tif`
with automatic format detection. A preset can store `out-format=auto` and
`tiff-alpha=premultiplied`; capture then needs a TIFF filename or
`--out-format tiff`. PNG/WebP combinations fail before capture or destination
creation, with instructions to choose TIFF or straight alpha. The picker
enables TIFF alpha only for saved TIFF, and choosing a non-TIFF format resets
the mode to Straight. Clipboard-only mode ignores an inherited preset's TIFF
alpha mode and conflicts with an explicit `--tiff-alpha`.

Without `-o`, alphashot saves `YYYY-MM-DD-HH-MM-SS-window-title.png` in the
current directory, using local time and a shortened slug of the title.
`--out-format webp` or `tiff` changes the default extension. `--out-dir DIR`
places automatic names in that folder; an explicit `--out` takes precedence.

Alphashot writes each file atomically and replaces an existing file only with
`--force`. Automatic names get the same protection. Backdrop raw composites default to
`capture-white.png` and `capture-black.png`; change the prefix with
`--raw-prefix`. The final image path must differ from both raw paths,
even with `--force`.

## Repeated capture

Use `--repeat` to select a window once and capture it immediately. Change the
application, then return to the terminal and press Enter for the next shot.
Press q or Esc to finish successfully, or Ctrl+C to cancel. Settings, including
changes made in the picker, stay the same for every shot. The window title can
change; closing the target or replacing it ends the session with an error and
instructions to select again. Backdrop captures restore the desktop before
waiting for another shot. WGC leaves foreground unchanged and restores any
temporary geometry changes before waiting for another shot.
`--post-pick-delay` applies before every capture.

Saved shots use numbered names: `-o notes.png` produces `notes-0001.png`,
`notes-0002.png`, and so on. Without `-o`, the session keeps the initial
timestamp/title as its base name. Existing numbers are skipped, including a
number used by either raw composite. Raw files use the same number, for example
`capture-0001-white.png` and `capture-0001-black.png`. Sessions never overwrite
files, even with `--force`; an atomic no-clobber write also protects files
created by another process during capture. A write failure ends the session
and keeps captures already saved.

`--repeat --clipboard-only` replaces the clipboard on every shot without
creating files or directories. `--editor` opens each saved shot. Repeated
capture requires terminal stdin and stderr; stdout can be redirected to collect
saved paths. Terminal raw mode is active only while waiting for the next key.

## Clipboard

After saving, alphashot replaces the Windows clipboard with the recovered image
and a file reference. Paste into an image editor or chat to use the image, or
into Explorer to copy the saved file. `--no-clipboard` leaves the clipboard alone.
Listing windows or displays, cancelling, and failed saves leave it alone too.

The clipboard offers PNG first, then a 32-bit Windows DIBV5 bitmap, both with
alpha and capture DPI. It always offers PNG even when the saved file is WebP or
TIFF. Offering both image formats follows
[Chromium's Windows clipboard strategy](https://chromium.googlesource.com/chromium/src/+/main/ui/base/clipboard/clipboard_win.cc).
The receiving app chooses which format to use and whether to preserve alpha.

The [file reference](https://learn.microsoft.com/en-us/windows/win32/shell/clipboard#cf_hdrop)
uses an absolute Unicode path and requests copying, so it works regardless of
the directory alphashot was started from. Keep the saved file in place until
you paste it. The image data remains available after alphashot exits, even if
the saved file is removed.

If another application keeps the clipboard busy, alphashot retries briefly,
then warns on stderr. The image is already saved, and clipboard failures do
not change a successful capture's exit code.

Use `--clipboard-only` to copy the transparent image without creating a file.
It offers the same PNG and DIBV5 image formats, with alpha and capture DPI,
but no file reference. This mode cannot be combined with `--out`, `--out-dir`,
`--tiff-alpha`, `--raw-dir`, `--no-clipboard`, `--editor` or `--editor-arg`.
Its confirmation goes to stderr,
leaving stdout empty. A clipboard failure makes this capture fail.

## Opening an editor

Pass an executable with `--editor` to open the saved transparent image after
capture:

```powershell
alphashot --window-title "Notepad" --editor mspaint.exe
```

Use `--editor-arg ARG` repeatedly for extra arguments, including flags such as
`--editor-arg=--new-window` when supported by your editor. Alphashot passes each
argument directly and appends the absolute saved path as one final argument.
On Windows, use an `.exe` or an extensionless executable name; `.bat` and `.cmd`
wrappers are rejected. The editor's input and output are disconnected from the
terminal, so use an editor that opens its own window. Launch failures warn on
stderr and leave the successful capture and its exit code intact.

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
