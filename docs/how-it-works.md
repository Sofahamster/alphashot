# How it works

## Two backdrops, one alpha solve

Alphashot places a temporary backdrop window behind the target and captures the
target twice: once over white, once over black. An opaque window pixel looks
the same in both captures. A shadow pixel looks lighter over white than over
black, and the difference gives its alpha. Alphashot solves for straight alpha
and color in linear light and crops the canvas to the visible pixels.

By default the client area comes from the black-background frame and stays
opaque. Rounded frame corners and the exterior shadow still use the white/black
solve. A blinking caret or an animated tab therefore cannot punch holes in the
image, and the two frames need no alignment.

For translucent client content, pass `--transparent-client` and keep the content
static during the capture. Alphashot cannot reconstruct blur materials that
depend on what sits behind the window.

`--no-crop`, `--crop-threshold` and `--margin` control the output canvas.

## Placement and shadow padding

DWM shadows extend beyond the window frame, so alphashot needs room around the
window. It moves a window near a monitor edge inward until the shadow fits in
the workspace, keeps its size, and restores the original position afterwards,
including after failures and Ctrl+C. Maximized windows get no exterior shadow
padding. If a window and its shadow do not fit, resize the window or use a
larger [high-DPI display](high-dpi.md). Avoid moving or resizing the window while
the capture runs.

Alphashot keeps foreground permission before it switches applications. Windows
can still refuse to restore the previous foreground window if you click another
application in the meantime.

`--pad-mode shadow` uses calibrated DWM geometry. At 96 DPI, shadow styles 2 to
5 reserve 64/32/64/96 pixels around the frame, style 1 reserves 16/8/16/24, and
style 0 reserves none. Alphashot scales these values with DPI and adds four
pixels by default. `--shadow-style-hint 0..5` overrides the inferred style, and
`--pad-mode uniform --pad N` requests equal padding on all sides.

## Menus

Alphashot includes open native Windows menus and cascading submenus, even when
they extend past the parent window. Their geometry must stay the same between
the two captures. An open menu blocks automatic repositioning and high-DPI
moves, because moving its owner would close it. Move the window before you open
the menu. Custom menus that do not use the standard Win32 menu class are not
detected.

## Backdrop verification

Alphashot checks the outer rim of each capture, excluding known window and menu
areas, to confirm the backdrop color. It retries four times before it gives up.
Keep other windows out of the capture area: alphashot cannot detect a window
that overlaps the target's own pixels.

## Elevated windows

Windows can reject backdrop placement behind an elevated application. Alphashot
then places its backdrop at the top of the normal window stack and shows it only
if the target is still above it. If Windows refuses that ordering too, alphashot
stops with an explanation. Launch alphashot from an administrator terminal, or
run the target without elevation; alphashot does not request elevation itself.
Microsoft documents the
[integrity-level restrictions](https://learn.microsoft.com/en-us/troubleshoot/power-platform/power-automate/desktop-flows/ui-automation/uipi-issues).

## Limits

Alphashot tolerates small client animations. Video, animated non-client
decorations, protected content and changing translucent content can still
produce wrong results. The canvas limit is 40 million pixels, and recovery
buffers use a lot of memory at large sizes. Restricted desktops can block window
activation. Moving a window between displays with different DPI still needs
hands-on testing with an active scaled display.

Alphashot has no GUI, persistent configuration, background service or upload
feature.
