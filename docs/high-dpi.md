# High-DPI capture

`--high-dpi` uses an active display scaled above 100%, temporarily moving the
window when needed so the application renders at that DPI. The image
has the higher resolution from the start; alphashot does not enlarge it afterwards.

## Set up a virtual display

Without a scaled physical monitor, install
[Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver)
using the [upstream installation instructions](https://github.com/VirtualDrivers/Virtual-Display-Driver#-installation).

**Quick setup:** Open Windows Terminal or PowerShell **as administrator** and
run the project's documented WinGet command:

```powershell
winget install --id=VirtualDrivers.Virtual-Display-Driver -e
```

Then enable the virtual monitor and set its scale in Windows Display settings:

1. In Windows Settings > System > Display, select the virtual display and choose
   **Extend desktop to this display**.
2. Pick a large resolution, such as 3840x2160, and set **Scale** to **200%**.
   Resolution alone at 100% does not make applications render at a higher DPI.
   Use SDR; HDR capture is untested.
3. Run the capture with `--high-dpi`. Alphashot reports the display it
   chose. If a prerequisite is missing, the error lists each active display's
   name, adapter, scale and resolution, with a link to the driver setup.
4. To choose a display yourself, pass its Windows device name
   (`alphashot --list-displays` shows the names):

   ```powershell
   alphashot --window-title "Notepad" --high-dpi --high-dpi-display '\\.\DISPLAY2' -o notepad-2x.png
   ```

Without a display name, alphashot picks the active display with the highest DPI
and breaks ties by workspace size. A scaled physical display works as well. An
installed driver without an extended display is not enough; check Device
Manager and the display settings if the display is missing.
The project's Virtual Driver Control app provides installation and configuration
options. Alphashot does not install drivers or change display settings for you.

With `--backend wgc`, ordinary captures stay on the current monitor. Explicit
`--high-dpi` temporarily moves the window to the scaled display without
activating it, then restores its original placement and size. Automatic
selection skips the move if the window already uses the highest available
DPI; a named display requests that particular monitor. This is useful for a
virtual display, where manually placing and retrieving a window is awkward.

## Behavior

Alphashot checks for a missing display or 100% scaling before it moves the
window. The application must support per-monitor DPI: if it reports a different
DPI than expected after the move, alphashot stops and restores the placement.
It does not fall back to ordinary DPI.

Without `--client-size`, WGC scales the window's physical size proportionally
when moving to a different DPI. Both backends may reduce it to fit the
destination workspace, which can change its layout. The backdrop backend
includes the shadow in this fit; WGC fits the window frame. Close open menus
first.

`--client-size 1200x800` requests exactly 1200 by 800 physical client pixels on
the capture monitor. Alphashot moves to the high-DPI monitor first, then sizes
the client area. The frame, shadow and padding are outside those dimensions,
so the resulting image can be larger. This changes the application's layout
at the monitor's actual DPI; it does not resample or upscale existing pixels.

If the application constrains the size, alphashot reports the requested and
actual dimensions and stops. During relocation, if the full capture cannot fit
the monitor's workspace, it stops instead of shrinking an explicitly requested
client area. WGC does not require a fully visible surface when resizing in
place, so it preserves position rather than fitting that case to the workspace.
The original placement and geometry are restored after success, cancellation
or an error. WGC rejects minimized windows and geometry changes to maximized
or snapped windows; restore those windows first. Already-satisfied geometry
does not require a temporary move or resize. The backdrop backend also
restores minimized and maximized placement. An open native menu must be closed
before changing geometry, because resizing dismisses it.
