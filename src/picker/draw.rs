//! Terminal rendering for the picker: a cell-grid `Frame`, layout, and incremental output.
//!
//! Each `draw` call builds a complete `Frame` in memory. `Frame::present` writes only the cells
//! that differ from the previous frame, inside a synchronized update. Unchanged cells are never
//! rewritten, because overwriting a cell erases the Sixel image the terminal holds there. The image
//! is sent again only when its pane or its encoded data (compared by `Arc` identity) changes, or on
//! resize, after the covered cells are repainted. A resize clears the screen and redraws everything.
//!
//! A `Frame` is a grid of `(char, ContentStyle)` cells with an optional cursor position and an
//! optional graphic. Drawing helpers write through `Frame::put` and never touch the terminal.
//! `draw` also returns a `View` that records the screen rectangles of rows, buttons, and settings
//! fields for `View::mouse`.
//!
//! A double-width glyph occupies two cells: the glyph, then the sentinel `WIDE` (`'\0'`), which is
//! never printed. `Frame::put` rejects zero-width characters and blanks the orphaned half of any
//! wide glyph it overwrites or splits, so the grid never holds half a glyph. `present` redraws both
//! halves together. `sanitized` drops zero-width characters from untrusted text such as window
//! titles and escapes control characters with `escape_debug`, so a title cannot inject terminal
//! escape sequences.
//!
//! The minimum size is 60x12; `draw` fails below that. From 108 columns and enough height, the wide
//! layout puts a 32-column sidebar with details, thumbnail, and settings beside the list. Otherwise
//! the stacked layout puts a details panel below the list and shows no thumbnail. The settings
//! modal is centered, and the thumbnail is hidden while the modal would cover it.
//!
//! Colors are 24-bit on a dark base. With a non-empty `NO_COLOR`, the selected row uses reverse
//! video.

use std::{
    io::Write,
    time::{Duration, Instant},
};

use anyhow::{Result, ensure};
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    event::{KeyCode, MouseButton, MouseEvent, MouseEventKind},
    queue,
    style::{Color, ContentStyle, PrintStyledContent, ResetColor, SetBackgroundColor, Stylize},
    terminal::{BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate},
};
use unicode_width::UnicodeWidthChar;

use super::{
    Picker,
    settings::{Editor, Field},
};
use crate::Cli;

/// Palette. `BASE` is the screen background, `PANEL` and `STRIPE` alternate list row
/// backgrounds, `SELECTED` marks the current row, and `TEAL`, `VIOLET`, `GOLD`, `TEXT` and `MUTED`
/// are foreground colors.
const BASE: Color = Color::Rgb {
    r: 15,
    g: 19,
    b: 25,
};
const PANEL: Color = Color::Rgb {
    r: 19,
    g: 25,
    b: 33,
};
const STRIPE: Color = Color::Rgb {
    r: 23,
    g: 30,
    b: 39,
};
const SELECTED: Color = Color::Rgb {
    r: 32,
    g: 58,
    b: 66,
};
const TEXT: Color = Color::Rgb {
    r: 218,
    g: 226,
    b: 235,
};
const MUTED: Color = Color::Rgb {
    r: 139,
    g: 156,
    b: 175,
};
const TEAL: Color = Color::Rgb {
    r: 103,
    g: 211,
    b: 190,
};
const VIOLET: Color = Color::Rgb {
    r: 186,
    g: 164,
    b: 235,
};
const GOLD: Color = Color::Rgb {
    r: 234,
    g: 190,
    b: 124,
};

/// A rectangle in terminal cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Area {
    x: u16,
    y: u16,
    w: u16,
    h: u16,
}

impl Area {
    /// Returns whether the cell `(x, y)` lies inside the rectangle.
    fn contains(self, x: u16, y: u16) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
    /// Returns whether two rectangles overlap.
    fn intersects(self, other: Self) -> bool {
        self.x < other.x + other.w
            && other.x < self.x + self.w
            && self.y < other.y + other.h
            && other.y < self.y + self.h
    }
}

/// One complete screen image. `cells` is row-major with `columns * rows` entries. `graphic`
/// is the thumbnail to show, which `present` writes after the cells.
pub(super) struct Frame {
    columns: u16,
    rows: u16,
    cells: Vec<(char, ContentStyle)>,
    cursor: Option<(u16, u16)>,
    graphic: Option<(Area, std::sync::Arc<super::thumbnail::Encoded>)>,
}

impl Frame {
    /// Creates a blank frame filled with spaces on the `BASE` background.
    fn new(columns: u16, rows: u16) -> Self {
        Self {
            columns,
            rows,
            cells: vec![
                (' ', ContentStyle::new().on(BASE));
                usize::from(columns) * usize::from(rows)
            ],
            cursor: None,
            graphic: None,
        }
    }

    /// Writes one glyph at column `x` of row `y` and writes `WIDE` into the second cell of a
    /// double-width glyph. Any wide glyph that the write overlaps or splits loses its other half
    /// (replaced by a space), and zero-width glyphs and glyphs that do not fit are ignored.
    fn put(&mut self, x: usize, y: u16, c: char, style: ContentStyle) {
        let row = usize::from(y) * usize::from(self.columns);
        let cells = &mut self.cells[row..row + usize::from(self.columns)];
        let w = cell_width(c);
        if w == 0 || x + w > cells.len() {
            return;
        }
        for i in x..x + w {
            if cells[i].0 == WIDE {
                cells[i - 1].0 = ' ';
            } else if cells.get(i + 1).is_some_and(|cell| cell.0 == WIDE) {
                cells[i + 1].0 = ' ';
            }
        }
        cells[x] = (c, style);
        if w == 2 {
            cells[x + 1] = (WIDE, style);
        }
    }

    /// Writes the cells that differ from `previous` to `stream` as one buffered, synchronized
    /// update. With no previous frame or a changed size, it clears the screen and writes everything.
    /// Unchanged cells are skipped, including those under a Sixel image, so the image survives.
    fn present(&self, previous: Option<&Self>, stream: &mut impl Write) -> Result<()> {
        let resized =
            previous.is_none_or(|old| old.columns != self.columns || old.rows != self.rows);
        let changed_image = match (&self.graphic, previous.and_then(|old| old.graphic.as_ref())) {
            (Some((area, image)), Some((old_area, old_image))) => {
                area != old_area || !std::sync::Arc::ptr_eq(image, old_image)
            }
            (None, None) => false,
            _ => true,
        };
        let changed = |x: u16, y: u16| {
            let index = usize::from(y) * usize::from(self.columns) + usize::from(x);
            resized
                || (changed_image
                    && (self
                        .graphic
                        .as_ref()
                        .is_some_and(|(area, _)| area.contains(x, y))
                        || previous
                            .and_then(|old| old.graphic.as_ref())
                            .is_some_and(|(area, _)| area.contains(x, y))))
                || previous.is_some_and(|old| old.cells[index] != self.cells[index])
        };
        // Both halves of a wide glyph are redrawn together.
        let dirty = |x: u16, y: u16| {
            let index = usize::from(y) * usize::from(self.columns) + usize::from(x);
            changed(x, y)
                || (self.cells[index].0 == WIDE && changed(x - 1, y))
                || (x + 1 < self.columns && self.cells[index + 1].0 == WIDE && changed(x + 1, y))
        };
        let mut out = Vec::new();
        queue!(out, BeginSynchronizedUpdate, Hide)?;
        if resized {
            queue!(
                out,
                ResetColor,
                SetBackgroundColor(BASE),
                Clear(ClearType::All)
            )?;
        }
        for y in 0..self.rows {
            let mut x = 0;
            while x < self.columns {
                let index = usize::from(y) * usize::from(self.columns) + usize::from(x);
                if !dirty(x, y) {
                    x += 1;
                    continue;
                }
                let start = x;
                let style = self.cells[index].1;
                let mut value = String::new();
                while x < self.columns {
                    let cell =
                        self.cells[usize::from(y) * usize::from(self.columns) + usize::from(x)];
                    if cell.1 != style || !dirty(x, y) {
                        break;
                    }
                    // Stop at unchanged cells to leave an existing Sixel image untouched.
                    if cell.0 != WIDE {
                        value.push(cell.0);
                    }
                    x += 1;
                }
                queue!(
                    out,
                    MoveTo(start, y),
                    PrintStyledContent(style.apply(value))
                )?;
            }
        }
        if (changed_image || resized)
            && let Some((area, image)) = &self.graphic
        {
            let (cell_w, cell_h) = crate::sixel::cell_pixels();
            let x = area.x
                + area
                    .w
                    .saturating_sub(image.pixels.0.div_ceil(cell_w) as u16)
                    / 2;
            let y = area.y
                + area
                    .h
                    .saturating_sub((image.pixels.1.div_ceil(6) * 6).div_ceil(cell_h) as u16)
                    / 2;
            queue!(out, MoveTo(x, y))?;
            out.extend_from_slice(image.data.as_bytes());
        }
        if let Some((x, y)) = self.cursor {
            queue!(out, MoveTo(x, y), Show)?;
        }
        queue!(out, ResetColor, EndSynchronizedUpdate)?;
        stream.write_all(&out)?;
        stream.flush()?;
        Ok(())
    }
}

/// Hit-testing data for the frame just drawn. `visible` is the number of list rows (the page
/// size for the next key), `offset` is the first visible match, and `modal` is true while the
/// settings editor captures the mouse.
pub(super) struct View {
    pub(super) visible: usize,
    offset: usize,
    rows: Area,
    buttons: Vec<(Area, KeyCode)>,
    fields: Vec<(Area, Field, isize)>,
    modal: bool,
}

impl View {
    /// Applies a mouse event to the picker. Settings fields open the editor or change a value,
    /// buttons return their key, the scrollbar column jumps, and rows select. A second click on
    /// a row within 400 ms returns Enter. The wheel moves three rows. While the editor is open,
    /// only left clicks are handled.
    pub(super) fn mouse(
        &self,
        picker: &mut Picker,
        args: &Cli,
        mouse: MouseEvent,
    ) -> Option<KeyCode> {
        if self.modal && mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return None;
        }
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                picker.last_click = None;
                picker.move_by(-3);
            }
            MouseEventKind::ScrollDown => {
                picker.last_click = None;
                picker.move_by(3);
            }
            MouseEventKind::Down(MouseButton::Left) => {
                for &(area, field, step) in &self.fields {
                    if area.contains(mouse.column, mouse.row) {
                        picker.last_click = None;
                        if let Some(editor) = &mut picker.editor {
                            editor.change(field, step);
                        } else {
                            picker.editor = Some(Editor::new(args, field));
                        }
                        return None;
                    }
                }
                for &(area, key) in &self.buttons {
                    if area.contains(mouse.column, mouse.row) {
                        return Some(key);
                    }
                }
                if self.modal {
                    return None;
                }
                // The scrollbar column only jumps when a scrollbar is drawn.
                if picker.matches.len() > self.visible
                    && mouse.column == self.rows.x + self.rows.w
                    && mouse.row >= self.rows.y
                    && mouse.row < self.rows.y + self.rows.h
                {
                    picker.selected = (usize::from(mouse.row - self.rows.y) * picker.matches.len()
                        / self.visible.saturating_sub(1).max(1))
                    .min(picker.matches.len().saturating_sub(1));
                    picker.last_click = None;
                } else if self.rows.contains(mouse.column, mouse.row) {
                    let index = self.offset + usize::from(mouse.row - self.rows.y);
                    if index < picker.matches.len() {
                        let twice = picker.last_click.is_some_and(|(old, time)| {
                            old == index && time.elapsed() < Duration::from_millis(400)
                        });
                        picker.selected = index;
                        picker.last_click = Some((index, Instant::now()));
                        return twice.then_some(KeyCode::Enter);
                    }
                }
            }
            _ => {}
        }
        None
    }
}

/// Foreground `color` on the panel background.
fn style(color: Color) -> ContentStyle {
    ContentStyle::new().with(color).on(PANEL)
}

/// Marks the right half of a double-width glyph; never printed.
const WIDE: char = '\0';

/// Terminal columns that `c` occupies: 0, 1 or 2 (control characters count as 0).
fn cell_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

/// Total terminal columns of `text`.
fn span(text: &str) -> usize {
    text.chars().map(cell_width).sum()
}

/// Makes untrusted text safe to print: drops zero-width characters and escapes control
/// characters, so every remaining character fills one or two cells. Quotes and backslashes
/// stay literal.
fn sanitized(text: &str) -> String {
    let mut clean = String::new();
    for c in text.chars() {
        if matches!(c, '\\' | '\'' | '"') {
            clean.push(c);
        } else if c.is_control() || cell_width(c) > 0 {
            clean.extend(c.escape_debug());
        }
    }
    clean
}

/// Sanitizes `text` and fits it to exactly `width` columns, ending with an ellipsis and
/// padded with spaces when it was cut.
fn clipped(text: &str, width: usize) -> String {
    let text = sanitized(text);
    if span(&text) <= width {
        return text;
    }
    let mut used = 0;
    let mut out: String = text
        .chars()
        .take_while(|&c| {
            used += cell_width(c);
            used < width
        })
        .collect();
    if width > 0 {
        out.push('…');
    }
    let pad = width.saturating_sub(span(&out));
    out.extend(std::iter::repeat_n(' ', pad));
    out
}

/// Sanitizes `text` and keeps its last `width` columns, so the end of a long query stays
/// visible beside the cursor.
fn tail(text: &str, width: usize) -> String {
    let mut used = 0;
    let mut chars: Vec<_> = sanitized(text)
        .chars()
        .rev()
        .take_while(|&c| {
            used += cell_width(c);
            used <= width
        })
        .collect();
    chars.reverse();
    chars.into_iter().collect()
}

/// Writes `value` clipped to `width` columns starting at `(x, y)`.
fn text(
    out: &mut Frame,
    x: u16,
    y: u16,
    width: u16,
    value: &str,
    style: ContentStyle,
) -> Result<()> {
    if y < out.rows {
        let end = (usize::from(x) + usize::from(width)).min(usize::from(out.columns));
        let mut column = usize::from(x);
        for c in clipped(value, usize::from(width)).chars() {
            if column + cell_width(c) > end {
                if column < end {
                    out.put(column, y, ' ', style);
                }
                break;
            }
            out.put(column, y, c, style);
            column += cell_width(c);
        }
    }
    Ok(())
}

/// Draws a bordered box with a `title` at the left and a `status` at the right.
fn panel(out: &mut Frame, area: Area, title: &str, status: &str, accent: Color) -> Result<()> {
    for row in area.y..area.y + area.h {
        text(
            out,
            area.x,
            row,
            area.w,
            &" ".repeat(usize::from(area.w)),
            style(TEXT),
        )?;
    }
    let rule = "─".repeat(usize::from(area.w - 2));
    text(
        out,
        area.x,
        area.y,
        area.w,
        &format!("╭{rule}╮"),
        style(accent).dim(),
    )?;
    text(
        out,
        area.x,
        area.y + area.h - 1,
        area.w,
        &format!("╰{rule}╯"),
        style(accent).dim(),
    )?;
    for row in area.y + 1..area.y + area.h - 1 {
        text(out, area.x, row, 1, "│", style(accent).dim())?;
        text(out, area.x + area.w - 1, row, 1, "│", style(accent).dim())?;
    }
    text(
        out,
        area.x + 2,
        area.y,
        area.w - 4,
        &format!(" {title} "),
        style(accent).bold(),
    )?;
    let status = clipped(
        status,
        usize::from(area.w.saturating_sub(span(title) as u16 + 9)),
    );
    let length = span(&status) as u16;
    if length > 0 {
        text(
            out,
            area.x + area.w - length - 3,
            area.y,
            length + 2,
            &format!(" {status} "),
            style(MUTED),
        )?;
    }
    Ok(())
}

/// Draws a clickable caption and registers its area in `view`, so a click sends `key`.
fn button(
    out: &mut Frame,
    view: &mut View,
    area: Area,
    caption: &str,
    key: KeyCode,
    primary: bool,
) -> Result<()> {
    let color = if primary { TEAL } else { TEXT };
    let background = if primary { SELECTED } else { STRIPE };
    text(
        out,
        area.x,
        area.y,
        area.w,
        &" ".repeat(usize::from(area.w)),
        style(color).on(background),
    )?;
    text(
        out,
        area.x + 1,
        area.y,
        area.w - 2,
        caption,
        style(color).on(background).bold(),
    )?;
    view.buttons.push((area, key));
    Ok(())
}

/// Draws `value` clipped to `width`, emphasizing characters that match the query words.
fn highlighted(
    out: &mut Frame,
    x: u16,
    y: u16,
    width: u16,
    value: &str,
    query: &str,
    normal: ContentStyle,
) -> Result<()> {
    let value = clipped(value, usize::from(width));
    let chars: Vec<_> = value.chars().collect();
    let lower: Vec<_> = chars
        .iter()
        .enumerate()
        .flat_map(|(i, c)| c.to_lowercase().map(move |c| (c, i)))
        .collect();
    let mut marks = vec![false; chars.len()];
    for word in query.to_lowercase().split_whitespace() {
        let needle: Vec<_> = word.chars().collect();
        let contiguous = lower
            .windows(needle.len())
            .position(|window| window.iter().map(|&(c, _)| c).eq(needle.iter().copied()));
        let positions = if let Some(start) = contiguous {
            Some((start..start + needle.len()).collect::<Vec<_>>())
        } else {
            let mut cursor = 0;
            needle
                .iter()
                .map(|c| {
                    let index = cursor + lower[cursor..].iter().position(|&(h, _)| h == *c)?;
                    cursor = index + 1;
                    Some(index)
                })
                .collect::<Option<Vec<_>>>()
        };
        if let Some(positions) = positions {
            for position in positions {
                marks[lower[position].1] = true;
            }
        }
    }
    let (mut start, mut column) = (0, x);
    while start < chars.len() {
        let mark = marks[start];
        let end = (start + 1..chars.len())
            .find(|&i| marks[i] != mark)
            .unwrap_or(chars.len());
        let color = if mark {
            normal.with(GOLD).bold()
        } else {
            normal
        };
        let segment: String = chars[start..end].iter().collect();
        let w = span(&segment) as u16;
        text(out, column, y, w, &segment, color)?;
        (start, column) = (end, column + w);
    }
    Ok(())
}

/// Splits sanitized `value` into at most `lines` lines of `width` columns each.
fn wrapped(value: &str, width: usize, lines: usize) -> Vec<String> {
    let mut chars = sanitized(value).chars().collect::<Vec<_>>();
    let mut output = Vec::new();
    while !chars.is_empty() && output.len() < lines {
        let line: String = chars.iter().collect();
        if span(&line) <= width || output.len() + 1 == lines {
            output.push(clipped(&line, width));
            break;
        }
        let mut used = 0;
        let fit = chars
            .iter()
            .position(|&c| {
                used += cell_width(c);
                used > width
            })
            .unwrap_or(chars.len());
        let split = chars[..fit]
            .iter()
            .rposition(|c| c.is_whitespace())
            .filter(|&i| span(&chars[..i].iter().collect::<String>()) > width / 2)
            .unwrap_or(fit);
        output.push(chars[..split].iter().collect());
        chars.drain(..split);
        while chars.first().is_some_and(|c| c.is_whitespace()) {
            chars.remove(0);
        }
    }
    output
}

/// Rectangle of the centered settings modal.
fn editor_area(columns: u16, rows: u16) -> Area {
    let (w, h) = ((columns - 4).min(66), (rows - 2).min(19));
    Area {
        x: (columns - w) / 2,
        y: (rows - h) / 2,
        w,
        h,
    }
}

/// Draws the settings modal over the frame and registers its mouse targets in `view`.
fn draw_editor(
    out: &mut Frame,
    view: &mut View,
    editor: &Editor,
    columns: u16,
    rows: u16,
) -> Result<()> {
    let area = editor_area(columns, rows);
    let (width, height) = (area.w, area.h);
    let first = area.y + if height >= 16 { 3 } else { 1 };
    let buttons = area.y + height - 3;
    view.modal = true;
    view.buttons.clear();
    view.fields.clear();
    panel(out, area, "capture settings", "", GOLD)?;
    if height >= 16 {
        text(
            out,
            area.x + 3,
            area.y + 1,
            width - 6,
            "Adjust this capture, then apply or cancel.",
            style(MUTED),
        )?;
    }
    let monochrome = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
    for (row, field) in Field::ALL.iter().enumerate() {
        let y = first + row as u16;
        let active = *field == editor.selected;
        let mut row_style = style(TEXT).on(if active { SELECTED } else { PANEL });
        if active && monochrome {
            row_style = row_style.reverse();
        }
        text(
            out,
            area.x + 1,
            y,
            width - 2,
            &" ".repeat(usize::from(width - 2)),
            row_style,
        )?;
        if active {
            text(out, area.x + 1, y, 1, "›", row_style.with(GOLD).bold())?;
        }
        text(
            out,
            area.x + 3,
            y,
            10,
            field.label(),
            row_style.with(if active { GOLD } else { MUTED }),
        )?;
        text(out, area.x + 14, y, 1, "‹", row_style.with(TEAL))?;
        text(
            out,
            area.x + 17,
            y,
            width - 22,
            &field.value(&editor.draft),
            row_style.with(TEAL).bold(),
        )?;
        text(out, area.x + width - 4, y, 1, "›", row_style.with(TEAL))?;
        view.fields.push((
            Area {
                x: area.x + 1,
                y,
                w: 13,
                h: 1,
            },
            *field,
            0,
        ));
        view.fields.push((
            Area {
                x: area.x + 14,
                y,
                w: 2,
                h: 1,
            },
            *field,
            -1,
        ));
        view.fields.push((
            Area {
                x: area.x + 16,
                y,
                w: width - 18,
                h: 1,
            },
            *field,
            1,
        ));
    }
    let help = (first + 6).min(buttons - 1);
    for (row, line) in wrapped(
        &editor.description(),
        usize::from(width - 6),
        usize::from((buttons - help).min(5)),
    )
    .iter()
    .enumerate()
    {
        text(
            out,
            area.x + 3,
            help + row as u16,
            width - 6,
            line,
            style(if editor.message.is_some() {
                GOLD
            } else {
                MUTED
            }),
        )?;
    }
    button(
        out,
        view,
        Area {
            x: area.x + 3,
            y: buttons,
            w: 17,
            h: 1,
        },
        "Enter Apply",
        KeyCode::Enter,
        true,
    )?;
    button(
        out,
        view,
        Area {
            x: area.x + width - 18,
            y: buttons,
            w: 15,
            h: 1,
        },
        "Esc Cancel",
        KeyCode::Esc,
        false,
    )?;
    text(
        out,
        area.x + 3,
        area.y + height - 2,
        width - 6,
        "↑↓ / Tab select  ←→ / Space change  F5 refresh",
        style(MUTED),
    )?;
    Ok(())
}

/// Builds the frame for the current picker state, writes the changes to `stream`, stores the
/// frame in `picker.frame` for the next diff, and returns the `View` for hit-testing.
///
/// Also keeps the selection in view and posts the thumbnail request to `picker.preview`; the
/// request is cleared when previews are disabled or the modal hides the pane. Fails when the
/// terminal is smaller than 60x12.
pub(super) fn draw(
    picker: &mut Picker,
    args: &Cli,
    stream: &mut impl Write,
    columns: u16,
    rows: u16,
) -> Result<View> {
    ensure!(
        columns >= 60 && rows >= 12,
        "Enlarge the terminal to at least 60 columns and 12 rows to select a window"
    );
    let width = columns.saturating_sub(if columns >= 84 { 4 } else { 0 });
    let height = rows.saturating_sub(if rows >= 28 { 2 } else { 0 });
    let area = Area {
        x: (columns - width) / 2,
        y: (rows - height) / 2,
        w: width,
        h: height,
    };
    let compact = height < 28;
    let body_y = area.y + if compact { 5 } else { 7 };
    let body_h = height - if compact { 8 } else { 10 };
    let sidebar = width >= 108 && body_h >= 14;
    let stacked = !sidebar && body_h >= 12;
    let windows = Area {
        x: area.x,
        y: body_y,
        w: if sidebar { width - 34 } else { width },
        h: if stacked { body_h - 5 } else { body_h },
    };
    let list = Area {
        x: windows.x + 1,
        y: windows.y + 2,
        w: windows.w - 3,
        h: windows.h - 3,
    };
    let visible = usize::from(list.h);
    picker.offset = picker
        .offset
        .min(picker.matches.len().saturating_sub(visible));
    if picker.selected < picker.offset {
        picker.offset = picker.selected;
    }
    if picker.selected >= picker.offset + visible {
        picker.offset = picker.selected + 1 - visible;
    }
    let offset = picker.offset;
    let mut view = View {
        visible,
        offset,
        rows: list,
        buttons: Vec::new(),
        fields: Vec::new(),
        modal: false,
    };
    let mut out = Frame::new(columns, rows);
    let mut preview_area = None;
    text(
        &mut out,
        area.x,
        area.y,
        16,
        "α  alphashot",
        style(TEAL).on(BASE).bold(),
    )?;
    text(
        &mut out,
        area.x + width - 21,
        area.y,
        21,
        &format!("WINDOW CAPTURE  {}", env!("CARGO_PKG_VERSION")),
        style(MUTED).on(BASE),
    )?;
    if !compact {
        text(
            &mut out,
            area.x,
            area.y + 1,
            width,
            "Choose a window. Keep its corners and shadows.",
            style(MUTED).on(BASE),
        )?;
    }
    let search = Area {
        x: area.x,
        y: area.y + if compact { 1 } else { 3 },
        w: width,
        h: 3,
    };
    panel(
        &mut out,
        search,
        "search",
        &format!(
            "{} {}",
            picker.matches.len(),
            if picker.matches.len() == 1 {
                "match"
            } else {
                "matches"
            }
        ),
        TEAL,
    )?;
    text(
        &mut out,
        search.x + 2,
        search.y + 1,
        2,
        "›",
        style(TEAL).bold(),
    )?;
    let query = tail(&picker.query, usize::from(width - 8));
    if picker.query.is_empty() {
        text(
            &mut out,
            search.x + 4,
            search.y + 1,
            width - 6,
            "Type a process name or window title…",
            style(MUTED),
        )?;
    } else {
        text(
            &mut out,
            search.x + 4,
            search.y + 1,
            width - 6,
            &query,
            style(TEXT),
        )?;
    }
    let position = if picker.matches.is_empty() {
        "0 / 0".into()
    } else {
        format!("{} / {}", picker.selected + 1, picker.matches.len())
    };
    panel(&mut out, windows, "windows", &position, TEAL)?;
    let process_w = (list.w / 4).clamp(16, 32);
    let pid_x = list.x + process_w + 3;
    let title_x = list.x + process_w + 12;
    text(
        &mut out,
        list.x + 2,
        windows.y + 1,
        process_w,
        "PROCESS",
        style(MUTED),
    )?;
    text(&mut out, pid_x, windows.y + 1, 8, "     PID", style(MUTED))?;
    text(
        &mut out,
        title_x,
        windows.y + 1,
        list.w - process_w - 12,
        "WINDOW TITLE",
        style(MUTED),
    )?;
    let monochrome = std::env::var_os("NO_COLOR").is_some_and(|value| !value.is_empty());
    for (row, &index) in picker.matches.iter().skip(offset).take(visible).enumerate() {
        let window = &picker.windows[index];
        let selected = offset + row == picker.selected;
        let background = if selected {
            SELECTED
        } else if row % 2 == 0 {
            PANEL
        } else {
            STRIPE
        };
        let mut normal = style(TEXT).on(background);
        if selected && monochrome {
            normal = normal.reverse();
        }
        let y = list.y + row as u16;
        text(
            &mut out,
            list.x,
            y,
            list.w,
            &" ".repeat(usize::from(list.w)),
            normal,
        )?;
        if selected {
            text(&mut out, list.x, y, 1, "›", normal.with(TEAL).bold())?;
        }
        highlighted(
            &mut out,
            list.x + 2,
            y,
            process_w,
            window.process_name.as_deref().unwrap_or("<unknown>"),
            &picker.query,
            if selected {
                normal.with(TEAL).bold()
            } else {
                normal.with(TEAL)
            },
        )?;
        text(
            &mut out,
            pid_x,
            y,
            8,
            &format!("{:>8}", window.pid),
            normal.with(MUTED),
        )?;
        highlighted(
            &mut out,
            title_x,
            y,
            list.w - process_w - 13,
            &window.title,
            &picker.query,
            normal,
        )?;
    }
    if picker.matches.len() > visible {
        let thumb = (visible * visible / picker.matches.len()).max(1);
        let top = offset * (visible - thumb) / (picker.matches.len() - visible);
        for row in 0..visible {
            text(
                &mut out,
                list.x + list.w,
                list.y + row as u16,
                1,
                if row >= top && row < top + thumb {
                    "┃"
                } else {
                    "│"
                },
                style(if row >= top && row < top + thumb {
                    TEAL
                } else {
                    MUTED
                })
                .dim(),
            )?;
        }
    }
    if picker.matches.is_empty() {
        text(
            &mut out,
            list.x + 2,
            list.y,
            list.w - 4,
            "No matching windows",
            style(GOLD).bold(),
        )?;
        if list.h >= 3 {
            text(
                &mut out,
                list.x + 2,
                list.y + 2,
                list.w - 4,
                "Edit the search or press Ctrl+U to clear it.",
                style(MUTED),
            )?;
        }
    }
    if sidebar || stacked {
        let details = if sidebar {
            Area {
                x: area.x + width - 32,
                y: body_y,
                w: 32,
                h: if body_h >= 26 {
                    body_h - 17
                } else {
                    body_h - 7
                },
            }
        } else {
            Area {
                x: area.x,
                y: windows.y + windows.h + 1,
                w: width,
                h: 4,
            }
        };
        panel(&mut out, details, "selected window", "", VIOLET)?;
        if let Some(&index) = picker.matches.get(picker.selected) {
            let window = &picker.windows[index];
            if sidebar {
                text(
                    &mut out,
                    details.x + 2,
                    details.y + 1,
                    28,
                    window.process_name.as_deref().unwrap_or("<unknown>"),
                    style(VIOLET).bold(),
                )?;
                let title = wrapped(&window.title, 28, usize::from((details.h - 7).min(5)));
                for (row, line) in title.iter().enumerate() {
                    text(
                        &mut out,
                        details.x + 2,
                        details.y + 2 + row as u16,
                        28,
                        line,
                        style(TEXT),
                    )?;
                }
                let top = details.y + 3 + title.len() as u16;
                let bottom = details.y + details.h - 6;
                if bottom >= top + 3 {
                    preview_area = Some(Area {
                        x: details.x + 2,
                        y: top,
                        w: details.w - 4,
                        h: (bottom - top).min(16),
                    });
                }
                for (row, value) in [
                    format!("{} × {} pixels", window.rect.width(), window.rect.height()),
                    format!("{}% scale  ·  {} DPI", window.dpi * 100 / 96, window.dpi),
                    format!("PID  {}", window.pid),
                    format!("HWND  0x{:X}", window.handle),
                ]
                .iter()
                .enumerate()
                {
                    text(
                        &mut out,
                        details.x + 2,
                        details.y + details.h - 5 + row as u16,
                        28,
                        value,
                        style(if row == 0 { TEXT } else { MUTED }),
                    )?;
                }
            } else {
                text(
                    &mut out,
                    details.x + 2,
                    details.y + 1,
                    width - 4,
                    &window.title,
                    style(TEXT),
                )?;
                text(
                    &mut out,
                    details.x + 2,
                    details.y + 2,
                    width - 4,
                    &format!(
                        "{} × {}  ·  {}%  ·  PID {}  ·  0x{:X}",
                        window.rect.width(),
                        window.rect.height(),
                        window.dpi * 100 / 96,
                        window.pid,
                        window.handle
                    ),
                    style(MUTED),
                )?;
            }
        }
        if sidebar {
            let show_settings = body_h >= 26;
            if show_settings {
                let settings = Area {
                    x: details.x,
                    y: details.y + details.h + 1,
                    w: 32,
                    h: 7,
                };
                panel(&mut out, settings, "capture settings", "F4 edit", GOLD)?;
                for (row, field) in Field::ALL.iter().enumerate() {
                    let y = settings.y + 1 + row as u16;
                    text(&mut out, settings.x + 2, y, 7, field.label(), style(MUTED))?;
                    text(
                        &mut out,
                        settings.x + 9,
                        y,
                        18,
                        &field.value(args),
                        style(TEXT),
                    )?;
                    text(&mut out, settings.x + 29, y, 1, "›", style(GOLD))?;
                    view.fields.push((
                        Area {
                            x: settings.x + 1,
                            y,
                            w: 30,
                            h: 1,
                        },
                        *field,
                        0,
                    ));
                }
            }
            let modes = Area {
                x: details.x,
                y: body_y + body_h - if show_settings { 8 } else { 6 },
                w: 32,
                h: if show_settings { 8 } else { 6 },
            };
            panel(&mut out, modes, "pick mode", "", TEAL)?;
            for (row, (caption, key, primary)) in [
                ("Enter  Capture selection".to_owned(), KeyCode::Enter, true),
                ("F2     Click a window".to_owned(), KeyCode::F(2), false),
                (
                    format!("F3     Cursor in {}s", args.pick_delay),
                    KeyCode::F(3),
                    false,
                ),
                ("F4     Capture settings".to_owned(), KeyCode::F(4), false),
            ]
            .iter()
            .enumerate()
            {
                button(
                    &mut out,
                    &mut view,
                    Area {
                        x: modes.x + 2,
                        y: modes.y + 1 + row as u16,
                        w: 28,
                        h: 1,
                    },
                    caption,
                    *key,
                    *primary,
                )?;
            }
            if show_settings {
                button(
                    &mut out,
                    &mut view,
                    Area {
                        x: modes.x + 2,
                        y: modes.y + 6,
                        w: 14,
                        h: 1,
                    },
                    "F5 Refresh",
                    KeyCode::F(5),
                    false,
                )?;
                button(
                    &mut out,
                    &mut view,
                    Area {
                        x: modes.x + 17,
                        y: modes.y + 6,
                        w: 13,
                        h: 1,
                    },
                    "Esc Quit",
                    KeyCode::Esc,
                    false,
                )?;
            }
        }
    }
    let footer = area.y + height - 2;
    if sidebar {
        text(
            &mut out,
            area.x,
            footer,
            width,
            "↑↓ Navigate   PgUp/PgDn Page   F4 Settings   F5 Refresh   Ctrl+U Clear   Esc Quit",
            style(MUTED).on(BASE),
        )?;
    } else {
        let mut x = area.x;
        for (caption, w, key, primary) in [
            ("Enter Capture", 15, KeyCode::Enter, true),
            ("F2 Click", 10, KeyCode::F(2), false),
            ("F3 Cursor", 11, KeyCode::F(3), false),
            ("F4 Edit", 9, KeyCode::F(4), false),
            ("Esc Quit", 10, KeyCode::Esc, false),
        ] {
            button(
                &mut out,
                &mut view,
                Area {
                    x,
                    y: footer,
                    w,
                    h: 1,
                },
                caption,
                key,
                primary,
            )?;
            x += w + 1;
        }
    }
    let destination = args
        .output
        .out
        .as_ref()
        .map(|p| format!("Save to  {}", p.display()))
        .unwrap_or_else(|| "Save to  current folder  ·  timestamp + window title".into());
    text(
        &mut out,
        area.x,
        footer + 1,
        width,
        &destination,
        style(MUTED).on(BASE),
    )?;
    let allowed = !args.output.no_preview && picker.preview.enabled;
    let hidden = picker.editor.is_some()
        && preview_area.is_some_and(|area| area.intersects(editor_area(columns, rows)));
    let request = if allowed && !hidden {
        preview_area.and_then(|area| {
            picker.matches.get(picker.selected).map(|&i| {
                let window = &picker.windows[i];
                super::thumbnail::Request {
                    window: super::thumbnail::Window {
                        handle: window.handle,
                        pid: window.pid,
                    },
                    pixels: crate::sixel::bounds(area.w, area.h),
                    epoch: picker.preview.epoch,
                }
            })
        })
    } else {
        None
    };
    picker.preview.request(request);
    if let Some(area) = preview_area {
        if allowed
            && !hidden
            && let Some(image) = &picker.preview.image
        {
            out.graphic = Some((area, image.clone()));
        } else if !hidden {
            let caption = if args.output.no_preview {
                "Preview disabled"
            } else if !allowed || picker.preview.failed {
                "Preview unavailable"
            } else {
                "Loading preview…"
            };
            text(
                &mut out,
                area.x,
                area.y,
                area.w,
                caption,
                style(MUTED).dim(),
            )?;
        }
    }
    if let Some(editor) = &picker.editor {
        draw_editor(&mut out, &mut view, editor, columns, rows)?;
    } else {
        out.cursor = Some((search.x + 4 + span(&query) as u16, search.y + 1));
    }
    out.present(picker.frame.as_ref(), stream)?;
    picker.frame = Some(out);
    Ok(view)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::picker::tests::window;
    use clap::Parser;
    use crossterm::event::KeyModifiers;

    #[test]
    fn preview_survives_filtering_and_uncovered_settings_and_suspends_when_obscured() {
        use std::sync::Arc;
        let mut args = Cli::parse_from(["alphashot"]);
        let mut picker = demo();
        picker.preview.enabled = true;
        draw(&mut picker, &args, &mut Vec::new(), 256, 68).unwrap();
        let request = picker.preview.request.unwrap();
        assert_eq!(request.window.handle, 20);
        let image = Arc::new(super::super::thumbnail::Encoded {
            data: "\x1bPq#0;2;100;100;100~\x1b\\".into(),
            pixels: (1, 6),
        });
        picker.preview.image = Some(image.clone());
        picker.preview.failed = false;
        let mut output = Vec::new();
        draw(&mut picker, &args, &mut output, 256, 68).unwrap();
        assert!(output.windows(2).any(|b| b == b"\x1bP"));
        picker.query = "code".into();
        picker.refilter();
        output.clear();
        draw(&mut picker, &args, &mut output, 256, 68).unwrap();
        assert!(Arc::ptr_eq(picker.preview.image.as_ref().unwrap(), &image));
        assert!(!output.windows(2).any(|b| b == b"\x1bP"));
        picker.key(KeyCode::F(4).into(), 10, &mut args).unwrap();
        output.clear();
        draw(&mut picker, &args, &mut output, 256, 68).unwrap();
        assert_eq!(picker.preview.request, Some(request));
        assert!(!output.windows(2).any(|b| b == b"\x1bP"));
        picker.key(KeyCode::Esc.into(), 10, &mut args).unwrap();

        draw(&mut picker, &args, &mut Vec::new(), 128, 48).unwrap();
        let resized = picker.preview.request.unwrap();
        assert_ne!(request.pixels, resized.pixels);
        assert_eq!(request.window, resized.window);
        assert!(picker.preview.image.is_none());
        picker.preview.image = Some(image);
        draw(&mut picker, &args, &mut Vec::new(), 128, 48).unwrap();
        picker.key(KeyCode::F(4).into(), 10, &mut args).unwrap();
        draw(&mut picker, &args, &mut Vec::new(), 128, 48).unwrap();
        assert!(
            picker.preview.request.is_none() && picker.frame.as_ref().unwrap().graphic.is_none()
        );
        picker.key(KeyCode::Esc.into(), 10, &mut args).unwrap();
        draw(&mut picker, &args, &mut Vec::new(), 128, 48).unwrap();
        assert_eq!(picker.preview.request, Some(resized));
        args.output.no_preview = true;
        draw(&mut picker, &args, &mut Vec::new(), 128, 48).unwrap();
        assert!(picker.preview.request.is_none());
        args.output.no_preview = false;
        draw(&mut picker, &args, &mut Vec::new(), 80, 24).unwrap();
        assert!(picker.preview.request.is_none());
    }

    #[test]
    fn unchanged_images_are_not_retransmitted_and_replacements_erase_the_old_area() {
        use std::sync::Arc;
        for rows in 3..=16 {
            let (_, budget) = crate::sixel::bounds(28, rows);
            assert_eq!(budget % 6, 0);
            assert!(budget <= u32::from(rows) * crate::sixel::cell_pixels().1);
        }
        let area = Area {
            x: 50,
            y: 4,
            w: 28,
            h: 8,
        };
        let encoded = Arc::new(super::super::thumbnail::Encoded {
            data: "\x1bPq#0;2;100;100;100~\x1b\\".into(),
            pixels: (1, 6),
        });
        let mut first = Frame::new(80, 24);
        first.graphic = Some((area, encoded.clone()));
        let mut output = Vec::new();
        first.present(None, &mut output).unwrap();
        assert!(output.windows(3).any(|bytes| bytes == b"\x1bPq"));

        let mut next = Frame::new(80, 24);
        next.graphic = Some((area, encoded.clone()));
        text(&mut next, 2, 1, 10, "filter", style(TEXT)).unwrap();
        output.clear();
        next.present(Some(&first), &mut output).unwrap();
        assert!(!output.windows(2).any(|bytes| bytes == b"\x1bP"));
        assert!(
            output.len() < 200,
            "Unrelated input should update only the changed text"
        );

        let mut modal = Frame::new(80, 24);
        text(&mut modal, 50, 5, 10, "Settings", style(TEXT)).unwrap();
        output.clear();
        modal.present(Some(&next), &mut output).unwrap();
        let cleared = String::from_utf8(output.clone()).unwrap();
        for row in area.y..area.y + area.h {
            assert!(
                cleared.contains(&format!("\x1b[{};{}H", row + 1, area.x + 1)),
                "Erase every old image row"
            );
        }
        assert!(!cleared.contains("\x1bP"));

        output.clear();
        next.present(Some(&modal), &mut output).unwrap();
        assert!(output.windows(2).any(|bytes| bytes == b"\x1bP"));
        let mut resized = Frame::new(100, 30);
        resized.graphic = Some((area, encoded));
        output.clear();
        resized.present(Some(&next), &mut output).unwrap();
        let resized = String::from_utf8(output).unwrap();
        assert!(resized.contains("\x1b[2J") && resized.contains("\x1bP"));
    }

    fn demo() -> Picker {
        let mut windows = vec![
            window(10, Some("WindowsTerminal.exe"), "PowerShell — alphashot"),
            window(
                20,
                Some("Code.exe"),
                "picker.rs — alphashot — Visual Studio Code",
            ),
            window(
                30,
                Some("firefox.exe"),
                "Alphashot documentation — Mozilla Firefox",
            ),
            window(40, Some("explorer.exe"), "Screenshots — File Explorer"),
            window(50, Some("Notepad.exe"), "Release notes — Notepad"),
            window(60, Some("mspaint.exe"), "Untitled — Paint"),
            window(70, Some("ApplicationFrameHost.exe"), "Calculator"),
            window(80, Some("Taskmgr.exe"), "Task Manager"),
        ];
        for i in 9..51 {
            windows.push(window(
                i * 10,
                Some("explorer.exe"),
                &format!("Workspace {i} — File Explorer"),
            ));
        }
        let mut picker = Picker::new(windows);
        picker.selected = 1;
        picker
    }

    // Replays emitted bytes on a terminal model that blanks orphaned wide-glyph halves.
    fn replay(screen: &mut Frame, bytes: &[u8]) {
        let output = String::from_utf8(bytes.to_vec()).unwrap();
        let mut chars = output.chars();
        let (mut x, mut y) = (0, 0);
        while let Some(c) = chars.next() {
            if c != '\x1b' {
                assert!(!c.is_control() && c != WIDE);
                assert!(x + cell_width(c) <= usize::from(screen.columns));
                screen.put(x, y, c, ContentStyle::new());
                x += cell_width(c);
                continue;
            }
            match chars.next() {
                Some('[') => {
                    let mut params = String::new();
                    for c in chars.by_ref() {
                        if !('@'..='~').contains(&c) {
                            params.push(c);
                            continue;
                        }
                        if c == 'H' {
                            let mut p = params.split(';').map(|v| v.parse::<usize>().unwrap_or(1));
                            y = p.next().unwrap_or(1) as u16 - 1;
                            x = p.next().unwrap_or(1) - 1;
                        } else if c == 'J' {
                            *screen = Frame::new(screen.columns, screen.rows);
                        }
                        break;
                    }
                }
                Some('P') => while chars.next().is_some_and(|c| c != '\\') {},
                _ => {}
            }
        }
    }

    fn glyphs(frame: &Frame) -> Vec<char> {
        frame.cells.iter().map(|cell| cell.0).collect()
    }

    fn line(frame: &Frame, y: u16, from: u16, to: u16) -> String {
        let row = usize::from(y) * usize::from(frame.columns);
        frame.cells[row + usize::from(from)..row + usize::from(to)]
            .iter()
            .map(|cell| cell.0)
            .filter(|&c| c != WIDE)
            .collect()
    }

    #[test]
    fn wide_and_zero_width_text_is_measured_in_terminal_columns() {
        assert_eq!(clipped("日本語 🎉 notes", 4), "日… ");
        assert_eq!(clipped("日本語 🎉 notes", 5), "日本…");
        assert_eq!(clipped("a\u{301}\u{200b}b\u{fe0f}", 5), "ab");
        assert_eq!(tail("ab日本", 5), "b日本");
        assert_eq!(wrapped("日本語 🎉 notes", 8, 3), ["日本語", "🎉 notes"]);

        let mut first = Frame::new(20, 1);
        text(&mut first, 2, 0, 10, "日本語", style(TEXT)).unwrap();
        let mut screen = Frame::new(20, 1);
        let mut output = Vec::new();
        first.present(None, &mut output).unwrap();
        replay(&mut screen, &output);
        assert_eq!(glyphs(&screen), glyphs(&first));

        let mut next = Frame::new(20, 1);
        text(&mut next, 2, 0, 10, "日本語", style(TEXT)).unwrap();
        text(&mut next, 5, 0, 1, "x", style(TEXT)).unwrap();
        text(&mut next, 18, 0, 5, "日本", style(TEXT)).unwrap();
        assert_eq!(
            line(&next, 0, 0, 20),
            format!("  日 x語{}日", " ".repeat(10))
        );
        text(&mut next, 6, 0, 1, "y", style(TEXT)).unwrap();
        text(&mut next, 19, 0, 1, "語", style(TEXT)).unwrap();
        assert_eq!(line(&next, 0, 0, 20), format!("  日 xy{}…", " ".repeat(12)));
        output.clear();
        next.present(Some(&first), &mut output).unwrap();
        replay(&mut screen, &output);
        assert_eq!(glyphs(&screen), glyphs(&next));
        assert!(String::from_utf8(output).unwrap().contains("\x1b[1;5H"));
    }

    #[test]
    fn wide_titles_clip_highlight_and_redraw_by_display_column() {
        let args = Cli::parse_from(["alphashot"]);
        let mut picker = demo();
        let title = "日本語 🎉 notes ".repeat(12);
        picker.windows[0] = window(10, Some("メモ帳.exe"), &title);
        let mut screen = Frame::new(128, 48);
        let mut output = Vec::new();
        let view = draw(&mut picker, &args, &mut output, 128, 48).unwrap();
        replay(&mut screen, &output);
        let frame = picker.frame.as_ref().unwrap();
        assert_eq!(glyphs(&screen), glyphs(frame));
        let list = view.rows;
        let process_w = (list.w / 4).clamp(16, 32);
        let (title_x, title_w, end) = (
            list.x + process_w + 12,
            list.w - process_w - 13,
            list.x + list.w,
        );
        let expected = clipped(&title, usize::from(title_w));
        assert_eq!(span(&expected), usize::from(title_w));
        assert_eq!(line(frame, list.y, title_x, title_x + title_w), expected);
        assert_eq!(line(frame, list.y, end - 1, end + 1), " ┃");
        assert_eq!(
            line(frame, list.y, list.x + 2, list.x + 2 + process_w).trim_end(),
            "メモ帳.exe"
        );

        picker.query = "本語 notes".into();
        picker.refilter();
        output.clear();
        draw(&mut picker, &args, &mut output, 128, 48).unwrap();
        replay(&mut screen, &output);
        let frame = picker.frame.as_ref().unwrap();
        assert_eq!(glyphs(&screen), glyphs(frame));
        let row = usize::from(list.y) * 128;
        let gold: Vec<_> = (title_x..end)
            .filter(|&x| frame.cells[row + usize::from(x)].1.foreground_color == Some(GOLD))
            .map(|x| x - title_x)
            .collect();
        assert_eq!(gold, [2, 3, 4, 5, 10, 11, 12, 13, 14]);

        picker.query = "taskmgr".into();
        picker.refilter();
        output.clear();
        draw(&mut picker, &args, &mut output, 128, 48).unwrap();
        replay(&mut screen, &output);
        let frame = picker.frame.as_ref().unwrap();
        assert_eq!(glyphs(&screen), glyphs(frame));
        assert!(!glyphs(frame).contains(&WIDE));
    }

    #[test]
    fn wide_search_queries_place_the_cursor_after_their_last_column() {
        let args = Cli::parse_from(["alphashot"]);
        let mut picker = demo();
        let (x, y) = (2 + 4, 1 + 3 + 1);
        picker.query = "日本🎉".into();
        draw(&mut picker, &args, &mut Vec::new(), 128, 48).unwrap();
        assert_eq!(picker.frame.as_ref().unwrap().cursor, Some((x + 6, y)));
        picker.query = format!("a{}", "日".repeat(70));
        draw(&mut picker, &args, &mut Vec::new(), 128, 48).unwrap();
        let frame = picker.frame.as_ref().unwrap();
        assert_eq!(frame.cursor, Some((x + 116, y)));
        assert_eq!(line(frame, y, x, x + 116), "日".repeat(58));
    }

    #[test]
    fn larger_terminals_show_more_rows_and_titles_and_preserve_selection_on_resize() {
        let args = Cli::parse_from(["alphashot"]);
        let mut picker = demo();
        picker.query = "workspace".into();
        picker.refilter();
        picker.selected = picker.matches.len() - 1;
        let selected = picker.matches[picker.selected];
        let small = draw(&mut picker, &args, &mut Vec::new(), 128, 48).unwrap();
        assert!(small.offset > 0);

        let large = draw(&mut picker, &args, &mut Vec::new(), 256, 68).unwrap();
        assert!(large.visible > small.visible);
        assert!(large.rows.w > small.rows.w);
        assert!(large.rows.w >= 192 && large.visible > 40);
        assert_eq!(
            large.offset, 0,
            "All filtered rows fit in the larger terminal"
        );
        assert_eq!(picker.matches[picker.selected], selected);
        assert_eq!(picker.query, "workspace");

        let resized = draw(&mut picker, &args, &mut Vec::new(), 80, 24).unwrap();
        assert_eq!(picker.matches[picker.selected], selected);
        assert!(picker.selected >= resized.offset);
        assert!(picker.selected < resized.offset + resized.visible);
        assert!(resized.visible < small.visible);
    }

    #[test]
    fn layout_and_mouse_targets_follow_terminal_size() {
        let args = Cli::parse_from(["alphashot"]);
        for (columns, rows) in [(60, 12), (80, 24), (110, 26), (128, 48), (200, 80)] {
            let mut picker = demo();
            let view = draw(&mut picker, &args, &mut Vec::new(), columns, rows).unwrap();
            assert!(view.visible > 0);
            assert!(view.rows.x + view.rows.w < columns && view.rows.y + view.rows.h < rows);
            let click = |column, row| MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            };
            assert_eq!(
                view.mouse(&mut picker, &args, click(view.rows.x, view.rows.y)),
                None
            );
            assert_eq!(picker.selected, view.offset);
            assert_eq!(
                view.mouse(&mut picker, &args, click(view.rows.x, view.rows.y)),
                Some(KeyCode::Enter)
            );
            for &(area, key) in &view.buttons {
                assert!(area.x + area.w <= columns && area.y + area.h <= rows);
                assert_eq!(
                    view.mouse(&mut picker, &args, click(area.x + 1, area.y)),
                    Some(key)
                );
            }
            picker.last_click = None;
            let row =
                view.rows.y + view.rows.h.min((picker.matches.len() - view.offset) as u16) - 1;
            view.mouse(&mut picker, &args, click(view.rows.x, row));
            let redraw = draw(&mut picker, &args, &mut Vec::new(), columns, rows).unwrap();
            assert_eq!(
                redraw.offset, view.offset,
                "Clicking a visible row must keep it under the mouse"
            );
            assert_eq!(
                redraw.mouse(&mut picker, &args, click(redraw.rows.x, row)),
                Some(KeyCode::Enter)
            );
            picker.query = "nothing matches this".into();
            picker.refilter();
            let empty = draw(&mut picker, &args, &mut Vec::new(), columns, rows).unwrap();
            assert_eq!(
                empty.mouse(&mut picker, &args, click(empty.rows.x, empty.rows.y)),
                None
            );
        }
    }

    #[test]
    fn settings_mouse_controls_apply_or_cancel_without_capturing() {
        for (columns, rows) in [(60, 12), (80, 24), (110, 26), (128, 48), (200, 80)] {
            let mut args = Cli::parse_from(["alphashot"]);
            let mut picker = demo();
            let click = |column, row| MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            };
            let view = draw(&mut picker, &args, &mut Vec::new(), columns, rows).unwrap();
            if let Some(&(area, field, _)) = view.fields.first() {
                assert_eq!(view.mouse(&mut picker, &args, click(area.x, area.y)), None);
                assert_eq!(picker.editor.as_ref().unwrap().selected, field);
            } else {
                picker
                    .key(KeyCode::F(4).into(), view.visible, &mut args)
                    .unwrap();
            }
            let view = draw(&mut picker, &args, &mut Vec::new(), columns, rows).unwrap();
            assert!(view.modal);
            for &(area, _, _) in &view.fields {
                assert!(area.x + area.w <= columns && area.y + area.h <= rows);
            }
            for &(area, _) in &view.buttons {
                assert!(area.x + area.w <= columns && area.y + area.h <= rows);
            }
            assert_eq!(view.mouse(&mut picker, &args, click(0, 0)), None);
            assert_eq!(picker.selected, 1);
            let (cursor, _, _) = view
                .fields
                .iter()
                .find(|(_, field, step)| *field == Field::Cursor && *step == 1)
                .unwrap();
            assert_eq!(
                view.mouse(&mut picker, &args, click(cursor.x, cursor.y)),
                None
            );
            assert!(picker.editor.as_ref().unwrap().draft.include_cursor);
            assert!(!args.include_cursor);
            let (apply, _) = view
                .buttons
                .iter()
                .find(|(_, key)| *key == KeyCode::Enter)
                .unwrap();
            let key = view
                .mouse(&mut picker, &args, click(apply.x, apply.y))
                .unwrap();
            assert_eq!(
                picker.key(key.into(), view.visible, &mut args).unwrap(),
                None
            );
            assert!(args.include_cursor && picker.editor.is_none());

            picker
                .key(KeyCode::F(4).into(), view.visible, &mut args)
                .unwrap();
            let view = draw(&mut picker, &args, &mut Vec::new(), columns, rows).unwrap();
            view.mouse(&mut picker, &args, click(cursor.x, cursor.y));
            assert!(!picker.editor.as_ref().unwrap().draft.include_cursor);
            let (cancel, _) = view
                .buttons
                .iter()
                .find(|(_, key)| *key == KeyCode::Esc)
                .unwrap();
            let key = view
                .mouse(&mut picker, &args, click(cancel.x, cancel.y))
                .unwrap();
            assert_eq!(
                picker.key(key.into(), view.visible, &mut args).unwrap(),
                None
            );
            assert!(args.include_cursor && picker.editor.is_none());
            assert_eq!(picker.selected, 1);
        }
    }

    #[test]
    fn settings_use_the_requested_capture_and_output_options() {
        let args = Cli::parse_from([
            "alphashot",
            "-o",
            r"C:\Screenshots\window.webp",
            "--include-cursor",
            "--transparent-client",
            "--pick-delay",
            "5",
        ]);
        let mut frame = Vec::new();
        draw(&mut demo(), &args, &mut frame, 128, 48).unwrap();
        let frame = String::from_utf8(frame).unwrap();
        for text in [
            "WebP · lossless",
            "Included",
            "Full window",
            "Cursor in 5s",
            r"C:\Screenshots\window.webp",
        ] {
            assert!(frame.contains(text), "Missing {text}");
        }
        assert_eq!(clipped("safe\\path\x1b[2J", 50), "safe\\path\\u{1b}[2J");
    }

    #[test]
    #[ignore = "Exports synthetic picker frames into target/picker-previews for visual inspection"]
    fn export_picker_previews() {
        let args = Cli::parse_from(["alphashot"]);
        let directory =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/picker-previews");
        std::fs::create_dir_all(&directory).unwrap();
        let mut picker = demo();
        for (name, width, height) in [
            ("wide", 128, 48),
            ("narrow", 80, 24),
            ("compact", 60, 12),
            ("maximized", 256, 68),
            ("window-preview", 128, 48),
            ("search", 128, 48),
        ] {
            if name == "window-preview" {
                picker.preview.enabled = true;
                draw(&mut picker, &args, &mut Vec::new(), width, height).unwrap();
                let (w, h) = picker.preview.request.unwrap().pixels;
                let source = image::RgbaImage::from_fn(800, 600, |x, y| {
                    let color = if y < 45 {
                        [32, 58, 66]
                    } else if x < 180 {
                        [26, 33, 44]
                    } else if (80..540).contains(&y) && y % 36 < 7 && x < 650 {
                        [103, 211, 190]
                    } else {
                        [19, 25, 33]
                    };
                    image::Rgba([color[0], color[1], color[2], 255])
                });
                let image = crate::sixel::fit(&source, w, h);
                picker.preview.image =
                    Some(std::sync::Arc::new(super::super::thumbnail::Encoded {
                        data: crate::sixel::encode(&image).unwrap(),
                        pixels: (image.width(), image.height()),
                    }));
                picker.preview.failed = false;
            }
            if name == "search" {
                picker.query = "code".into();
                picker.refilter();
            }
            picker.frame = None;
            let mut frame = Vec::new();
            draw(&mut picker, &args, &mut frame, width, height).unwrap();
            std::fs::write(directory.join(format!("{name}.ansi")), frame).unwrap();
        }
        picker.editor = Some(Editor::new(&args, Field::Format));
        picker.editor.as_mut().unwrap().change(Field::Format, 1);
        picker.editor.as_mut().unwrap().change(Field::Cursor, 1);
        for (name, width, height) in [
            ("settings", 128, 48),
            ("settings-compact", 60, 12),
            ("settings-scale", 80, 24),
        ] {
            if name == "settings-scale" {
                picker.editor.as_mut().unwrap().selected = Field::Scale;
            }
            picker.frame = None;
            let mut frame = Vec::new();
            draw(&mut picker, &args, &mut frame, width, height).unwrap();
            std::fs::write(directory.join(format!("{name}.ansi")), frame).unwrap();
        }
    }
}
