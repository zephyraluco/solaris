//! Stable text anchors independent of viewport rows. Layout whitespace is not an identity.
use std::collections::HashMap;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// A rendered row belonging to a stable content part.
#[derive(Debug, Clone)]
pub struct DocumentRow {
    pub part: u64,
    pub text: String,
}

/// A position within a part's visible graphemes. Spaces retain an affinity to
/// the preceding content so indentation and line wrapping can change safely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextAnchor {
    pub part: u64,
    pub grapheme: usize,
    pub whitespace: u16,
}

#[derive(Debug)]
struct Glyph {
    anchor: TextAnchor,
    column: u16,
    width: u16,
}

/// Shared immutable layout. Rebuild on content/layout changes, not every frame.
#[derive(Debug, Default)]
pub struct TextDocument {
    rows: Vec<DocumentRow>,
    glyphs: Vec<Vec<Glyph>>,
    positions: HashMap<(u64, usize), (usize, u16, u16)>,
}

impl TextDocument {
    pub fn new(rows: Vec<DocumentRow>) -> Self {
        let mut counts = HashMap::<u64, usize>::new();
        let mut positions = HashMap::new();
        let mut glyphs = Vec::with_capacity(rows.len());
        for (row, line) in rows.iter().enumerate() {
            let mut column = 0u16;
            let count = counts.entry(line.part).or_default();
            let mut cells = Vec::new();
            let mut whitespace = 0u16;
            for grapheme in line.text.graphemes(true) {
                let width = UnicodeWidthStr::width(grapheme) as u16;
                if grapheme.chars().all(char::is_whitespace) {
                    whitespace = whitespace.saturating_add(width);
                    cells.push(Glyph {
                        anchor: TextAnchor {
                            part: line.part,
                            grapheme: *count,
                            whitespace,
                        },
                        column,
                        width,
                    });
                } else {
                    whitespace = 0;
                    let anchor = TextAnchor {
                        part: line.part,
                        grapheme: *count,
                        whitespace: 0,
                    };
                    positions.insert((line.part, *count), (row, column, width));
                    cells.push(Glyph {
                        anchor,
                        column,
                        width,
                    });
                    *count += 1;
                }
                column = column.saturating_add(width);
            }
            glyphs.push(cells);
        }
        Self {
            rows,
            glyphs,
            positions,
        }
    }

    pub fn rows(&self) -> &[DocumentRow] {
        &self.rows
    }

    /// Hit-test in document coordinates, snapping inside a wide grapheme.
    pub fn anchor(&self, row: usize, column: u16) -> Option<TextAnchor> {
        let line = self.rows.get(row)?;
        let glyphs = self.glyphs.get(row)?;
        if let Some(glyph) = glyphs.iter().find(|glyph| {
            column >= glyph.column && column < glyph.column.saturating_add(glyph.width)
        }) {
            return Some(glyph.anchor);
        }
        // Padding to the right stays attached to this row's final glyph.
        let glyph = glyphs.last()?;
        Some(TextAnchor {
            part: line.part,
            grapheme: glyph.anchor.grapheme,
            whitespace: column.saturating_sub(glyph.column).saturating_add(1),
        })
    }

    /// Project an anchor to this layout. Hidden content has no screen position.
    pub fn locate(&self, anchor: TextAnchor) -> Option<(usize, u16, u16)> {
        if anchor.whitespace == 0 {
            return self.positions.get(&(anchor.part, anchor.grapheme)).copied();
        }
        // Match whitespace explicitly when possible; the fallback keeps padding
        // after the previous content when wrapping inserted or removed it.
        for (row, glyphs) in self.glyphs.iter().enumerate() {
            if let Some(glyph) = glyphs.iter().find(|glyph| glyph.anchor == anchor) {
                return Some((row, glyph.column, glyph.width));
            }
        }
        let previous = anchor
            .grapheme
            .checked_sub(1)
            .and_then(|offset| self.positions.get(&(anchor.part, offset)))?;
        Some((
            previous.0,
            previous
                .1
                .saturating_add(previous.2)
                .saturating_add(anchor.whitespace - 1),
            1,
        ))
    }

    /// Extract the currently visible layout between two semantic endpoints.
    pub fn selected_text(&self, a: TextAnchor, b: TextAnchor) -> Option<String> {
        let a = self.locate(a)?;
        let b = self.locate(b)?;
        let (start, end) = if (a.0, a.1) <= (b.0, b.1) {
            (a, b)
        } else {
            (b, a)
        };
        let mut lines = Vec::new();
        for row in start.0..=end.0 {
            let from = if row == start.0 { start.1 } else { 0 };
            let to = if row == end.0 {
                end.1.saturating_add(end.2)
            } else {
                u16::MAX
            };
            let mut text = String::new();
            let mut column = 0u16;
            for grapheme in self.rows[row].text.graphemes(true) {
                let width = UnicodeWidthStr::width(grapheme) as u16;
                if column >= from && column < to {
                    text.push_str(grapheme);
                }
                column = column.saturating_add(width);
            }
            if row == end.0 && to > column && to != u16::MAX {
                text.push_str(&" ".repeat(usize::from(to - column.max(from))));
            }
            lines.push(text);
        }
        let trimmed = lines
            .iter()
            .map(|line| line.trim_end())
            .collect::<Vec<_>>()
            .join("\n");
        Some(if trimmed.trim_end().is_empty() {
            lines.join("\n")
        } else {
            trimmed.trim_end().to_string()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn doc(lines: &[&str]) -> TextDocument {
        TextDocument::new(
            lines
                .iter()
                .map(|text| DocumentRow {
                    part: 7,
                    text: text.to_string(),
                })
                .collect(),
        )
    }
    #[test]
    fn wide_text_anchors_survive_reflow_and_hidden_parts() {
        let first = doc(&["hello 中文 world"]);
        let anchor = first.anchor(0, 7).expect("inside wide glyph");
        let wrapped = doc(&["hello", "中文 world"]);
        assert_eq!(wrapped.locate(anchor), Some((1, 0, 2)));
        assert_eq!(TextDocument::default().locate(anchor), None);
        assert_eq!(first.locate(anchor), Some((0, 6, 2)));
    }
    #[test]
    fn document_positions_are_not_limited_to_u16_rows() {
        let document = TextDocument::new(
            (0..70000)
                .map(|part| DocumentRow {
                    part,
                    text: "text".into(),
                })
                .collect(),
        );
        let anchor = document.anchor(69999, 1).expect("anchor");
        assert_eq!(document.locate(anchor), Some((69999, 1, 1)));
    }
}
