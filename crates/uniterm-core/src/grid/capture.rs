//! Owned checkpoint input, detached from the live grid and its arenas.
//!
//! Capturing copies compact cells, not one allocated string per cell. The
//! persistence worker resolves them later without borrowing a live grid.

use super::{stored_cell_is_default_blank, Cell, Color, Grid, StoredCell, StoredLine};
use serde::ser::{Serialize, SerializeSeq, SerializeStruct, Serializer};

/// Immutable terminal content that can cross the runtime seam without sharing
/// a live grid. See `docs/05-session-persistence.md`.
#[derive(Clone, Debug, Default)]
pub struct GridCapture {
    cells: Vec<Cell>,
    /// End offsets into `cells`, paired with the physical line's wrap flag.
    lines: Vec<(usize, bool)>,
    clusters: Vec<String>,
    underline_colors: Vec<Color>,
}

// The custom serializer writes the existing StoredLine/StoredCell schema,
// borrowing arena text and using a stack buffer for ordinary characters.
impl Serialize for GridCapture {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.lines.len()))?;
        let mut start = 0;
        for &(end, wrapped) in &self.lines {
            sequence.serialize_element(&CapturedLine {
                capture: self,
                start,
                end,
                wrapped,
            })?;
            start = end;
        }
        sequence.end()
    }
}

struct CapturedLine<'a> {
    capture: &'a GridCapture,
    start: usize,
    end: usize,
    wrapped: bool,
}

impl Serialize for CapturedLine<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut line = serializer.serialize_struct("StoredLine", 2)?;
        line.serialize_field("cells", &CapturedCells(self))?;
        line.serialize_field("wrapped", &self.wrapped)?;
        line.end()
    }
}

struct CapturedCells<'a>(&'a CapturedLine<'a>);

#[derive(serde::Serialize)]
struct CapturedCell<'a> {
    text: &'a str,
    fg: Color,
    bg: Color,
    attrs: super::Attrs,
    underline_color: Color,
    width: u8,
    continuation: bool,
}

impl Serialize for CapturedCells<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let line = self.0;
        let capture = line.capture;
        let mut end = line.end;
        if !line.wrapped {
            while end > line.start {
                let mut scalar = [0; 4];
                let cell = capture.cell(&capture.cells[end - 1], &mut scalar);
                if cell.text != " "
                    || cell.fg != Color::Default
                    || cell.bg != Color::Default
                    || cell.attrs != super::Attrs::NONE
                    || cell.underline_color != Color::Default
                    || cell.width != 1
                    || cell.continuation
                {
                    break;
                }
                end -= 1;
            }
        }
        let mut sequence = serializer.serialize_seq(Some(end - line.start))?;
        for cell in &capture.cells[line.start..end] {
            let mut scalar = [0; 4];
            sequence.serialize_element(&capture.cell(cell, &mut scalar))?;
        }
        sequence.end()
    }
}

impl Grid {
    /// Capture recent history with a handful of bulk allocations instead of
    /// resolving every grapheme on the core loop. The result owns all data it
    /// needs, including arena entries that the live grid may later compact.
    pub fn capture_lines(&self, max: usize) -> GridCapture {
        let start = self.total_lines().saturating_sub(max);
        let retained = (start..self.total_lines())
            .filter_map(|index| self.line(index))
            .map(|line| line.cells.len())
            .sum();
        let mut capture = GridCapture {
            cells: Vec::with_capacity(retained),
            lines: Vec::with_capacity(self.total_lines() - start),
            clusters: Vec::new(),
            underline_colors: Vec::new(),
        };
        let mut has_clusters = false;
        let mut has_underline_colors = false;
        for index in start..self.total_lines() {
            let Some(line) = self.line(index) else {
                continue;
            };
            let keep = if line.wrapped {
                line.cells.len()
            } else {
                line.cells
                    .iter()
                    .rposition(|cell| *cell != Cell::default())
                    .map_or(0, |index| index + 1)
            };
            let cells = &line.cells[..keep];
            has_clusters |= cells.iter().any(|cell| cell.cluster != 0);
            has_underline_colors |= cells.iter().any(|cell| cell.underline_color != 0);
            capture.cells.extend_from_slice(cells);
            capture.lines.push((capture.cells.len(), line.wrapped));
        }
        if has_clusters {
            capture.clusters.clone_from(&self.clusters);
        }
        if has_underline_colors {
            capture.underline_colors.clone_from(&self.underline_colors);
        }
        capture
    }
}

impl GridCapture {
    /// Preserve already recovered content without passing through a narrower
    /// viewport or adding recovery diagnostics to its durable history.
    pub fn from_stored_lines(lines: Vec<StoredLine>) -> Self {
        let mut capture = Self {
            clusters: vec![String::new()],
            underline_colors: vec![Color::Default],
            ..Self::default()
        };
        for line in lines {
            for saved in line.cells {
                let mut chars = saved.text.chars();
                let ch = chars.next().unwrap_or(' ');
                let cluster = if chars.next().is_some() {
                    capture.clusters.push(saved.text);
                    (capture.clusters.len() - 1) as u32
                } else {
                    0
                };
                let underline_color = capture
                    .underline_colors
                    .iter()
                    .position(|color| *color == saved.underline_color)
                    .unwrap_or_else(|| {
                        if capture.underline_colors.len() < 256 {
                            capture.underline_colors.push(saved.underline_color);
                            capture.underline_colors.len() - 1
                        } else {
                            0
                        }
                    }) as u8;
                capture.cells.push(Cell {
                    ch,
                    cluster,
                    underline_color,
                    fg: saved.fg,
                    bg: saved.bg,
                    attrs: saved.attrs,
                    width: if saved.continuation {
                        0
                    } else {
                        saved.width.max(1)
                    },
                    hyperlink: 0,
                });
            }
            capture.lines.push((capture.cells.len(), line.wrapped));
        }
        capture
    }

    fn cell<'a>(&'a self, cell: &Cell, scalar: &'a mut [u8; 4]) -> CapturedCell<'a> {
        CapturedCell {
            text: if cell.is_continuation() {
                ""
            } else if cell.cluster != 0 {
                self.clusters
                    .get(cell.cluster as usize)
                    .map(String::as_str)
                    .unwrap_or("�")
            } else {
                cell.ch.encode_utf8(scalar)
            },
            fg: cell.fg,
            bg: cell.bg,
            attrs: cell.attrs,
            underline_color: self
                .underline_colors
                .get(cell.underline_color as usize)
                .copied()
                .unwrap_or_default(),
            width: cell.width,
            continuation: cell.is_continuation(),
        }
    }

    /// Account for retained allocations without serializing on the hot path.
    /// Capacity, rather than length, keeps runtime backpressure conservative.
    pub fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            + self.cells.capacity() * std::mem::size_of::<Cell>()
            + self.lines.capacity() * std::mem::size_of::<(usize, bool)>()
            + self.clusters.capacity() * std::mem::size_of::<String>()
            + self.clusters.iter().map(String::capacity).sum::<usize>()
            + self.underline_colors.capacity() * std::mem::size_of::<Color>()
    }

    /// Expand detached cells into the existing durable schema on a worker.
    /// No grid handles or runtime-specific types enter the on-disk format.
    pub fn into_stored_lines(self) -> Vec<StoredLine> {
        let mut start = 0;
        self.lines
            .iter()
            .map(|&(end, wrapped)| {
                let mut cells: Vec<_> = self.cells[start..end]
                    .iter()
                    .map(|cell| StoredCell {
                        text: if cell.is_continuation() {
                            String::new()
                        } else if cell.cluster != 0 {
                            self.clusters
                                .get(cell.cluster as usize)
                                .cloned()
                                .unwrap_or_else(|| "�".into())
                        } else {
                            cell.ch.to_string()
                        },
                        fg: cell.fg,
                        bg: cell.bg,
                        attrs: cell.attrs,
                        underline_color: self
                            .underline_colors
                            .get(cell.underline_color as usize)
                            .copied()
                            .unwrap_or_default(),
                        width: cell.width,
                        continuation: cell.is_continuation(),
                    })
                    .collect();
                start = end;
                if !wrapped {
                    while cells.last().is_some_and(stored_cell_is_default_blank) {
                        cells.pop();
                    }
                }
                StoredLine { cells, wrapped }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovered_capture_retains_width_graphemes_styles_and_line_boundaries() {
        let lead = StoredCell {
            text: "👩‍💻".into(),
            fg: Color::Rgb(10, 20, 30),
            bg: Color::Default,
            attrs: super::super::Attrs::UNDERLINE,
            underline_color: Color::Rgb(40, 50, 60),
            width: 2,
            continuation: false,
        };
        let continuation = StoredCell {
            text: String::new(),
            width: 0,
            continuation: true,
            ..lead.clone()
        };
        let lines = vec![
            StoredLine {
                cells: (0..150)
                    .flat_map(|_| [lead.clone(), continuation.clone()])
                    .collect(),
                wrapped: true,
            },
            StoredLine {
                cells: vec![lead, continuation],
                wrapped: false,
            },
            StoredLine {
                cells: vec![],
                wrapped: false,
            },
        ];
        let capture = GridCapture::from_stored_lines(lines.clone());
        assert_eq!(capture.into_stored_lines(), lines);
        assert!(GridCapture::from_stored_lines(vec![])
            .into_stored_lines()
            .is_empty());
    }

    #[test]
    fn capture_is_detached_and_matches_resolved_export() {
        let mut grid = Grid::new(80, 5);
        for row in 0..20 {
            grid.set(
                0,
                4,
                Cell {
                    ch: char::from(b'a' + row),
                    ..Cell::default()
                },
            );
            grid.scroll_up(Cell::default());
        }
        let expected = grid.export_lines(12);
        let capture = grid.capture_lines(12);
        grid.scroll_up(Cell::default());
        drop(grid);
        assert_eq!(capture.into_stored_lines(), expected);
    }

    #[test]
    fn empty_capture_is_empty_and_does_not_dirty_the_grid() {
        let grid = Grid::new(80, 24);
        assert!(grid.capture_lines(0).into_stored_lines().is_empty());
        assert_eq!(
            grid.capture_lines(1000).into_stored_lines(),
            grid.export_lines(1000)
        );
        assert!(!grid.is_dirty());
    }
}
