# alphashot 0.2.0

## Capture workflows

- Save named presets and apply them from the CLI or picker. Presets cover output
  folders, formats, cursor, focus, margins, DPI, backend and shadow settings.
  Explicit command-line options take precedence.
- Use `--repeat` to select a window once and capture successive changes with the
  same settings. Captures receive numbered names without overwriting existing
  files; a closed or replaced target produces a clear error.
- Saved captures now copy the transparent image and file reference to the
  Windows clipboard. Use `--no-clipboard` to leave the clipboard unchanged.
- Use `--clipboard-only` to copy an image without creating a file or adding a
  file reference to the clipboard.
- Open saved images in a chosen editor with `--editor` and `--editor-arg`.
  Arguments are passed separately, and a launch failure preserves the saved image.
- Use `--out-dir` to set the folder for automatically named files.
- Request exact physical client dimensions with `--client-size 1280x800`.
  Alphashot restores the original geometry after success, cancellation or
  failure, and reports dimensions the application refuses.
- Combine `--process`, `--window-class` and `--window-title` filters with AND.
  Use `--wait SECONDS` to wait for a matching window. Ambiguous matches remain
  errors; alphashot never silently chooses the first candidate.

## Backends, shadows and DPI

- Select `--backend wgc` to capture covered or unfocused windows without
  activating them. WGC preserves the alpha supplied by Windows and omits the
  outer DWM shadow. Failures never fall back to foreground capture.
- Use `--no-shadow` to retain the title bar and chrome while cropping to the
  frame. Standard rounded frames use a DPI-aware, one-pixel feathered corner
  mask. This is a visual approximation; unsupported custom frames fail clearly.
- WGC supports automated high-DPI capture: when needed, it temporarily moves
  and proportionally scales the target on a higher-DPI display without
  activating it, then restores its original placement. The image is rendered
  at the destination DPI rather than enlarged afterwards.
- A new high-DPI setup guide covers Virtual Display Driver installation,
  extending the desktop and configuring display scaling.
- TIFF can store straight or premultiplied alpha with
  `--tiff-alpha straight|premultiplied` and matching metadata. Straight remains
  the default. PNG and WebP keep straight alpha; clipboard DIBV5 uses
  premultiplied RGB. Low-alpha premultiplication can reduce color precision.

## Fixes since 0.1.0

- Accept AltGr characters in the picker search.
- Preserve window proportions when resizing picker thumbnails.
- Ignore clicks in the scrollbar column when no scrollbar is displayed.
- Keep emoji and combining characters readable in window titles.
- Select top-level windows instead of child controls; invalid child handles
  report the containing window to use.
- Correct client-area coordinates for mirrored and right-to-left layouts.
- Recover SDR backdrop alpha and RGB in encoded sRGB, correcting exaggerated
  shadow opacity and antialiased frame edges.

## Requirements and limits

The backdrop backend remains the default. It requires Windows 10 1903 or newer;
WGC requires Windows 10 2004 or newer. Building requires Rust 1.88 or newer and
the MSVC build tools. Capture requires an interactive Windows desktop.

WGC geometry changes require a normal, unsnapped window. Minimized and protected
targets fail clearly. Material appearance comes from the application and
Windows: Mica is opaque, and Acrylic can depend on its background. HDR capture
is not validated. Rounded-corner masking is approximate.

Thanks to Alexander Frase for the fixes in
[PR #1](https://github.com/Sofahamster/alphashot/pull/1).

Download `alphashot.exe` below or run `cargo install alphashot`.
