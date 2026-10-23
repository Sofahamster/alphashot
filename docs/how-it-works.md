# How it works

## Two backdrops, one alpha solve

Alphashot places a temporary backdrop window behind the target and captures the
target twice: once over white, once over black. An opaque window pixel looks
the same in both captures. A shadow pixel looks lighter over white than over
black, and the difference gives its alpha. For the supported 8-bit SDR capture
path, alphashot solves alpha and straight color in encoded sRGB, matching the
DWM composition, and crops the canvas to the visible pixels.

RGB bytes are normalized to 0–1 without gamma decoding. Recovery computes
`alpha = 1 - median(white - black)` and `color = black / alpha` in that same
space; near-zero alpha becomes transparent black. Alpha is an opacity scalar
and never receives an sRGB transfer function. Linear-light recovery remains
available to library callers for explicitly linear-composited inputs. WGC
keeps its supplied alpha and unpremultiplies RGB instead of solving two passes.

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

## Excluding the DWM shadow

`--no-shadow` preserves the title bar, window chrome and translucent content,
and uses the actual extended frame bounds as the canvas. It does not change
the application's window styles or non-client rendering policy. Client pixels
remain in the alpha solve instead of being forced opaque; keep translucent
content still when using the backdrop backend.

Standard DWM captions use an approximate rounded mask. Windows 11 standard
frames use an 8-DIP radius with `DEFAULT` or `ROUND`, 4 DIPs with `ROUNDSMALL`,
and zero with `DONOTROUND`. Windows 10, maximized, snapped and remote-session
frames use square corners. Radii scale by the window's actual DPI; the feather
stays one physical pixel wide. Native captions with a fixed or resizable frame
are accepted only with DWM non-client rendering enabled, a matching caption
inset and no application-defined window region. Full-client custom chrome is
excluded even if it advertises standard caption styles.

| Window state | `--no-shadow` |
| --- | --- |
| Already maximized, non-layered window | Supported with square corners. A custom frame must stay maximized; standard native frames may be normalized by `--client-size` or high-DPI capture. |
| Standard native caption and frame | Supported with an approximate feathered corner mask; `--client-size` and high-DPI moves are allowed, with radius selection repeated after geometry changes. |
| Layered window with DWM non-client rendering disabled, no class drop-shadow style, and `DEFAULT` or `DONOTROUND` corner preference | Supported for this source class, including its per-pixel content alpha. |
| Explicit `ROUND` or `ROUNDSMALL` preference, unknown preference, or unavailable preference metadata on a non-maximized layered window | Refused; non-client rendering metadata alone does not exclude DWM corner residue. |
| Full-client custom chrome, application-defined region or unidentified frame | Fails before moving, resizing or activating the target. |
| Open native menu | Close the menu first. |

A rectangular crop retains shadow in rounded corner cutouts. The geometric
mask clears pixels outside the estimated corner and caps existing alpha along
its feather, rather than multiplying already antialiased alpha a second time.
Straight RGB and pixels outside the corner squares remain unchanged; cursor
layers are added afterwards. The corner's final pixel or two can differ from
Windows' exact border, and a little mixed shadow may remain along the feather.
This is an intentional visual approximation, not exact shadow separation.
Style flags and layout checks cannot prove how every custom application draws
its non-client area.

The SDR backdrop solve uses encoded sRGB for native frames, shadows and the
layered exception. `DEFAULT` follows compositor policy;
this support boundary is not a guarantee for every custom application or
future Windows compositor. Microsoft describes per-pixel layered windows as
outside automatic rounding and `DONOTROUND` as preventing system rounding in
its [corner-policy documentation](https://learn.microsoft.com/en-us/windows/apps/desktop/modernize/ui/apply-rounded-corners).


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

Alphashot has no GUI, background service or upload feature. Named capture
presets are its only persistent configuration.
