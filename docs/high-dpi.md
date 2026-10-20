# High-DPI capture

`--high-dpi` moves the window to an active display scaled above 100%,
so the application renders at that DPI. The image has the higher resolution
from the start; alphashot does not enlarge it afterwards.

## Set up a virtual display

Without a scaled physical monitor, install
[Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver)
with its Virtual Driver Control app, following the
[upstream installation instructions](https://github.com/VirtualDrivers/Virtual-Display-Driver#-installation).
Alphashot does not install drivers or change display settings for you.

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

## Behavior

Alphashot checks for a missing display or 100% scaling before it moves the
window. The application must support per-monitor DPI: if it reports a different
DPI than expected after the move, alphashot stops and restores the placement.
It does not fall back to ordinary DPI.

To fit the shadow in the destination workspace, alphashot may resize the window
for the duration of the capture, which can change its layout. Close open menus
first.
