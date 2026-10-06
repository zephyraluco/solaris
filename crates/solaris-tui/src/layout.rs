//! Flex-like stack layout: `basis` / `grow` / `shrink` / `min` / `max`.
//!
//! Mirrors pi-tui's `VStack` / `HStack` semantics, but as a pure geometry
//! function so both the framework and the application can use it.

use ratatui::layout::Rect;

/// Stack direction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

/// Preferred size of a stack entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Basis {
    /// Fixed number of cells.
    Cells(u16),
    /// Percentage of the stack extent.
    Percent(u16),
    /// Sized by the leftover space (and by `grow`).
    Auto,
}

/// One entry in a stack.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub basis: Basis,
    pub grow: u16,
    pub shrink: u16,
    pub min: u16,
    pub max: Option<u16>,
}

impl Entry {
    /// Fixed-height/width entry.
    pub fn px(cells: u16) -> Self {
        Self {
            basis: Basis::Cells(cells),
            grow: 0,
            shrink: 1,
            min: 0,
            max: None,
        }
    }

    /// Percentage-sized entry.
    pub fn percent(percent: u16) -> Self {
        Self {
            basis: Basis::Percent(percent),
            ..Self::px(0)
        }
    }

    /// Content-sized entry; grows unless `grow` is set.
    pub fn auto() -> Self {
        Self {
            basis: Basis::Auto,
            ..Self::px(0)
        }
    }

    /// Grow weight (0 = never takes leftover space).
    pub fn grow(mut self, weight: u16) -> Self {
        self.grow = weight;
        self
    }

    /// Shrink weight (0 = never gives up space).
    pub fn shrink(mut self, weight: u16) -> Self {
        self.shrink = weight;
        self
    }

    /// Lower bound in cells.
    pub fn min(mut self, cells: u16) -> Self {
        self.min = cells;
        self
    }

    /// Upper bound in cells.
    pub fn max(mut self, cells: u16) -> Self {
        self.max = Some(cells);
        self
    }
}

fn resolve_basis(basis: Basis, extent: i32) -> i32 {
    match basis {
        Basis::Cells(cells) => cells as i32,
        Basis::Percent(percent) => extent * percent as i32 / 100,
        Basis::Auto => 0,
    }
}

/// Split `area` into one sub-rect per entry, in order.
pub fn split(area: Rect, axis: Axis, entries: &[Entry]) -> Vec<Rect> {
    let count = entries.len();
    if count == 0 {
        return Vec::new();
    }

    let extent = match axis {
        Axis::Vertical => area.height as i32,
        Axis::Horizontal => area.width as i32,
    };

    let mut sizes: Vec<i32> = entries
        .iter()
        .map(|entry| resolve_basis(entry.basis, extent))
        .collect();

    let leftover = extent - sizes.iter().sum::<i32>();

    if leftover > 0 {
        let grow_total: i32 = entries.iter().map(|entry| entry.grow as i32).sum();
        if grow_total > 0 {
            let mut assigned = 0;
            for (idx, entry) in entries.iter().enumerate() {
                if entry.grow > 0 {
                    let share = leftover * entry.grow as i32 / grow_total;
                    sizes[idx] += share;
                    assigned += share;
                }
            }
            // Hand out rounding remainders one cell at a time.
            let mut remainder = leftover - assigned;
            let mut cursor = 0usize;
            let mut guard = 0usize;
            while remainder > 0 && guard < count * 4 {
                if entries[cursor % count].grow > 0 {
                    sizes[cursor % count] += 1;
                    remainder -= 1;
                }
                cursor += 1;
                guard += 1;
            }
        } else {
            // No grow weights: flexible entries share the space equally.
            let autos: Vec<usize> = entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.basis == Basis::Auto)
                .map(|(idx, _)| idx)
                .collect();
            if !autos.is_empty() {
                let each = leftover / autos.len() as i32;
                let mut remainder = leftover % autos.len() as i32;
                for idx in autos {
                    sizes[idx] += each;
                    if remainder > 0 {
                        sizes[idx] += 1;
                        remainder -= 1;
                    }
                }
            }
        }
    } else if leftover < 0 {
        let mut deficit = -leftover;
        let mut guard = 0usize;
        while deficit > 0 && guard < 64 {
            guard += 1;

            let weights: Vec<i32> = entries
                .iter()
                .enumerate()
                .map(|(idx, entry)| {
                    if sizes[idx] > entry.min as i32 {
                        entry.shrink as i32
                    } else {
                        0
                    }
                })
                .collect();
            let weight_total: i32 = weights.iter().sum();
            if weight_total == 0 {
                break;
            }

            let mut changed = false;
            for idx in 0..count {
                if deficit <= 0 {
                    break;
                }
                if weights[idx] == 0 {
                    continue;
                }
                // Ceiling division so every pass makes progress.
                let wanted = ((deficit * weights[idx] + weight_total - 1) / weight_total).max(1);
                let capacity = sizes[idx] - entries[idx].min as i32;
                let take = wanted.min(capacity).min(deficit);
                if take > 0 {
                    sizes[idx] -= take;
                    deficit -= take;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
    }

    for (idx, entry) in entries.iter().enumerate() {
        let mut size = sizes[idx].max(entry.min as i32).max(0);
        if let Some(max) = entry.max {
            size = size.min(max as i32);
        }
        sizes[idx] = size;
    }

    let mut rects = Vec::with_capacity(count);
    let mut offset = 0i32;
    for size in &sizes {
        let size = (*size).max(0);
        let rect = match axis {
            Axis::Vertical => Rect {
                x: area.x,
                y: area.y.saturating_add(offset as u16),
                width: area.width,
                height: size as u16,
            },
            Axis::Horizontal => Rect {
                x: area.x.saturating_add(offset as u16),
                y: area.y,
                width: size as u16,
                height: area.height,
            },
        };
        rects.push(rect);
        offset += size;
    }
    rects
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area(width: u16, height: u16) -> Rect {
        Rect::new(0, 0, width, height)
    }

    #[test]
    fn fixed_sizes_are_exact() {
        let rects = split(area(40, 10), Axis::Vertical, &[Entry::px(2), Entry::px(3)]);
        assert_eq!(rects[0].height, 2);
        assert_eq!(rects[1].height, 3);
        assert_eq!(rects[1].y, 2);
    }

    #[test]
    fn grow_absorbs_leftover() {
        let rects = split(
            area(40, 10),
            Axis::Vertical,
            &[Entry::auto().grow(1), Entry::px(3)],
        );
        assert_eq!(rects[0].height, 7);
        assert_eq!(rects[1].height, 3);
        assert_eq!(rects[1].y, 7);
    }

    #[test]
    fn grow_weights_split_proportionally() {
        let rects = split(
            area(40, 9),
            Axis::Vertical,
            &[Entry::auto().grow(1), Entry::auto().grow(2)],
        );
        assert_eq!(rects[0].height, 3);
        assert_eq!(rects[1].height, 6);
    }

    #[test]
    fn shrink_reduces_to_fit() {
        let rects = split(
            area(40, 5),
            Axis::Vertical,
            &[Entry::px(5), Entry::px(5).shrink(0)],
        );
        // Only the first entry shrinks.
        assert_eq!(rects[0].height, 0);
        assert_eq!(rects[1].height, 5);
    }

    #[test]
    fn min_and_max_clamp() {
        let rects = split(
            area(40, 10),
            Axis::Vertical,
            &[Entry::px(1).min(3), Entry::px(9).max(4)],
        );
        assert_eq!(rects[0].height, 3);
        assert_eq!(rects[1].height, 4);
    }

    #[test]
    fn percent_uses_extent() {
        let rects = split(
            area(40, 10),
            Axis::Vertical,
            &[Entry::percent(50), Entry::auto().grow(1)],
        );
        assert_eq!(rects[0].height, 5);
        assert_eq!(rects[1].height, 5);
    }

    #[test]
    fn horizontal_splits_widths() {
        let rects = split(
            area(10, 3),
            Axis::Horizontal,
            &[Entry::px(4), Entry::auto().grow(1)],
        );
        assert_eq!(rects[0].width, 4);
        assert_eq!(rects[1].width, 6);
        assert_eq!(rects[1].x, 4);
        assert_eq!(rects[0].height, 3);
    }
}
