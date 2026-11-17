//! Stable text anchors independent of viewport rows. Layout whitespace is not an identity.
use std::collections::HashMap;
use std::sync::Arc;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
    rows: Vec<Arc<DocumentRow>>,
    row_parts: Vec<(u64, usize)>,
    parts: HashMap<u64, PartPlacement>,
}

#[derive(Debug)]
struct PartPlacement {
    global_rows: Vec<usize>,
    index: Arc<PartIndex>,
}
#[derive(Debug, Default)]
struct PartIndex {
    rows: Vec<Arc<DocumentRow>>,
    glyphs: Vec<Vec<Glyph>>,
    positions: HashMap<usize, (usize, u16, u16)>,
    whitespace_positions: HashMap<TextAnchor, (usize, u16, u16)>,
}
impl PartIndex {
    fn new(rows: Vec<Arc<DocumentRow>>) -> Self {
        let mut count = 0usize;
        let mut positions = HashMap::new();
        let mut whitespace_positions = HashMap::new();
        let mut glyphs = Vec::with_capacity(rows.len());
        for (row, line) in rows.iter().enumerate() {
            let mut column = 0u16;
            let mut cells = Vec::new();
            let mut whitespace = 0u16;
            for grapheme in line.text.graphemes(true) {
                let width = UnicodeWidthStr::width(grapheme) as u16;
                if grapheme.chars().all(char::is_whitespace) {
                    whitespace = whitespace.saturating_add(width);
                    cells.push(Glyph {
                        anchor: TextAnchor {
                            part: line.part,
                            grapheme: count,
                            whitespace,
                        },
                        column,
                        width,
                    });
                } else {
                    whitespace = 0;
                    let anchor = TextAnchor {
                        part: line.part,
                        grapheme: count,
                        whitespace: 0,
                    };
                    positions.insert(count, (row, column, width));
                    cells.push(Glyph {
                        anchor,
                        column,
                        width,
                    });
                    count += 1;
                }
                column = column.saturating_add(width);
            }
            for glyph in &cells {
                if glyph.anchor.whitespace != 0 {
                    whitespace_positions.entry(glyph.anchor).or_insert((
                        row,
                        glyph.column,
                        glyph.width,
                    ));
                }
            }
            glyphs.push(cells);
        }
        Self {
            rows,
            glyphs,
            positions,
            whitespace_positions,
        }
    }
}

impl TextDocument {
    pub fn new(rows: Vec<DocumentRow>) -> Self {
        Self::assemble(rows.into_iter().map(Arc::new).collect(), None)
    }

    /// Keep the immutable prefix and its part indexes; only index changed parts.
    /// Old documents remain usable by selections held across a streaming update.
    pub fn with_suffix(&self, from: usize, suffix: Vec<DocumentRow>) -> Self {
        let mut rows = self.rows[..from.min(self.rows.len())].to_vec();
        rows.extend(suffix.into_iter().map(Arc::new));
        Self::assemble(rows, Some(self))
    }

    fn assemble(rows: Vec<Arc<DocumentRow>>, previous: Option<&Self>) -> Self {
        let mut grouped = HashMap::<u64, Vec<usize>>::new();
        let mut row_parts = Vec::with_capacity(rows.len());
        for (row, line) in rows.iter().enumerate() {
            let part = grouped.entry(line.part).or_default();
            row_parts.push((line.part, part.len()));
            part.push(row);
        }
        let parts = grouped
            .into_iter()
            .map(|(part, global_rows)| {
                let old = previous.and_then(|doc| doc.parts.get(&part));
                let index = match old {
                    Some(old)
                        if old.index.rows.len() == global_rows.len()
                            && old.index.rows.iter().zip(&global_rows).all(|(old, row)| {
                                Arc::ptr_eq(old, &rows[*row]) || old.text == rows[*row].text
                            }) =>
                    {
                        old.index.clone()
                    }
                    _ => Arc::new(PartIndex::new(
                        global_rows.iter().map(|row| rows[*row].clone()).collect(),
                    )),
                };
                (part, PartPlacement { global_rows, index })
            })
            .collect();
        Self {
            rows,
            row_parts,
            parts,
        }
    }

    pub fn rows(&self) -> &[Arc<DocumentRow>] {
        &self.rows
    }

    /// Hit-test in document coordinates, snapping inside a wide grapheme.
    pub fn anchor(&self, row: usize, column: u16) -> Option<TextAnchor> {
        let line = self.rows.get(row)?;
        let (part, local) = self.row_parts.get(row)?;
        let glyphs = self.parts.get(part)?.index.glyphs.get(*local)?;
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
        let placement = self.parts.get(&anchor.part)?;
        let index = &placement.index;
        let local = if anchor.whitespace == 0 {
            index.positions.get(&anchor.grapheme).copied()
        } else if let Some(position) = index.whitespace_positions.get(&anchor) {
            Some(*position)
        } else {
            let previous = anchor
                .grapheme
                .checked_sub(1)
                .and_then(|offset| index.positions.get(&offset))?;
            Some((
                previous.0,
                previous
                    .1
                    .saturating_add(previous.2)
                    .saturating_add(anchor.whitespace - 1),
                1,
            ))
        }?;
        Some((*placement.global_rows.get(local.0)?, local.1, local.2))
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
    fn streaming_suffix_reuses_history_indexes_and_preserves_old_documents() {
        let first = TextDocument::new(vec![
            DocumentRow {
                part: 1,
                text: "中文 é 👩‍💻".repeat(1000),
            },
            DocumentRow {
                part: 2,
                text: "tail".into(),
            },
        ]);
        let anchor = first.anchor(0, 1).unwrap();
        let next = first.with_suffix(
            1,
            vec![DocumentRow {
                part: 2,
                text: "tail appended".into(),
            }],
        );
        assert!(Arc::ptr_eq(&first.parts[&1].index, &next.parts[&1].index));
        assert!(!Arc::ptr_eq(&first.parts[&2].index, &next.parts[&2].index));
        assert_eq!(first.locate(anchor), next.locate(anchor));
        assert_eq!(first.rows()[1].text, "tail");
        let end = next.anchor(1, 12).unwrap();
        assert!(
            next.selected_text(anchor, end)
                .unwrap()
                .ends_with("tail appended")
        );
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
