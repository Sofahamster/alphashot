//! Captures a Windows window over a white and a black backdrop and recovers straight alpha and
//! color per pixel, so drop shadows and anti-aliased corners stay transparent.
//!
//! - [`capture`] takes the two composites, reports window, monitor and cursor data and holds the
//!   shared geometry types.
//! - [`alpha`] solves alpha and color and draws cursor overlays.
//! - [`output`] writes PNG, lossless WebP or TIFF through an atomic rename and builds file names.
//!
//! The API serves the `alphashot` CLI and may change in any 0.x release. Live capture is
//! Windows-only; on other targets the desktop functions in [`capture`] return an error.
//!
//! Method: <https://github.com/Sofahamster/alphashot/blob/main/docs/how-it-works.md>.
#![warn(missing_docs)]

pub mod alpha;
pub mod capture;
pub mod output;
