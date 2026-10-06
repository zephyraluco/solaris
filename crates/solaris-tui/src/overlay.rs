//! Overlay geometry: anchored, optionally percentage-sized modal regions.

use ratatui::layout::Rect;

/// Anchor point the overlay is positioned against.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Anchor {
    #[default]
    Center,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    TopCenter,
    BottomCenter,
    LeftCenter,
    RightCenter,
}

/// A size expressed in cells or as a percentage of the terminal extent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeValue {
    Cells(u16),
    Percent(u16),
}

impl SizeValue {
    /// Resolve against a terminal extent.
    pub fn resolve(self, extent: u16) -> u16 {
        match self {
            SizeValue::Cells(cells) => cells,
            SizeValue::Percent(percent) => (extent as u32 * percent as u32 / 100) as u16,
        }
    }
}

/// How an overlay should be sized and placed.
#[derive(Debug, Clone)]
pub struct OverlayOptions {
    pub width: Option<SizeValue>,
    pub height: Option<SizeValue>,
    pub min_width: u16,
    pub max_width: Option<SizeValue>,
    pub max_height: Option<SizeValue>,
    pub anchor: Anchor,
    pub offset_x: i16,
    pub offset_y: i16,
    pub margin: u16,
}

impl Default for OverlayOptions {
    fn default() -> Self {
        Self {
            width: Some(SizeValue::Percent(70)),
            height: None,
            min_width: 30,
            max_width: Some(SizeValue::Percent(90)),
            max_height: Some(SizeValue::Percent(80)),
            anchor: Anchor::Center,
            offset_x: 0,
            offset_y: 0,
            margin: 1,
        }
    }
}

impl OverlayOptions {
    pub fn centered() -> Self {
        Self::default()
    }

    pub fn width(mut self, width: SizeValue) -> Self {
        self.width = Some(width);
        self
    }

    pub fn height(mut self, height: SizeValue) -> Self {
        self.height = Some(height);
        self
    }

    pub fn min_width(mut self, cells: u16) -> Self {
        self.min_width = cells;
        self
    }

    pub fn max_height(mut self, height: SizeValue) -> Self {
        self.max_height = Some(height);
        self
    }

    pub fn anchor(mut self, anchor: Anchor) -> Self {
        self.anchor = anchor;
        self
    }

    pub fn margin(mut self, cells: u16) -> Self {
        self.margin = cells;
        self
    }
}

/// Resolve the overlay rectangle within `area`.
pub fn resolve(area: Rect, options: &OverlayOptions) -> Rect {
    if area.width == 0 || area.height == 0 {
        return Rect::new(area.x, area.y, 0, 0);
    }

    let margin = options.margin.min(area.width / 2).min(area.height / 2);
    let max_width = area.width.saturating_sub(margin * 2).max(1);
    let max_height = area.height.saturating_sub(margin * 2).max(1);

    let mut width = options
        .width
        .map(|value| value.resolve(area.width))
        .unwrap_or(max_width)
        .max(options.min_width)
        .min(max_width);
    if let Some(max) = options.max_width {
        width = width.min(max.resolve(area.width)).min(max_width);
    }

    let mut height = options
        .height
        .map(|value| value.resolve(area.height))
        .unwrap_or(max_height)
        .min(max_height);
    if let Some(max) = options.max_height {
        height = height.min(max.resolve(area.height)).min(max_height);
    }

    let horizontal_center = area.x as i32 + (area.width as i32 - width as i32) / 2;
    let vertical_center = area.y as i32 + (area.height as i32 - height as i32) / 2;

    let (mut x, mut y) = match options.anchor {
        Anchor::Center => (horizontal_center, vertical_center),
        Anchor::TopLeft => (area.x as i32 + margin as i32, area.y as i32 + margin as i32),
        Anchor::TopRight => (
            area.x as i32 + area.width as i32 - margin as i32 - width as i32,
            area.y as i32 + margin as i32,
        ),
        Anchor::BottomLeft => (
            area.x as i32 + margin as i32,
            area.y as i32 + area.height as i32 - margin as i32 - height as i32,
        ),
        Anchor::BottomRight => (
            area.x as i32 + area.width as i32 - margin as i32 - width as i32,
            area.y as i32 + area.height as i32 - margin as i32 - height as i32,
        ),
        Anchor::TopCenter => (horizontal_center, area.y as i32 + margin as i32),
        Anchor::BottomCenter => (
            horizontal_center,
            area.y as i32 + area.height as i32 - margin as i32 - height as i32,
        ),
        Anchor::LeftCenter => (area.x as i32 + margin as i32, vertical_center),
        Anchor::RightCenter => (
            area.x as i32 + area.width as i32 - margin as i32 - width as i32,
            vertical_center,
        ),
    };

    x += options.offset_x as i32;
    y += options.offset_y as i32;

    let max_x = area.x as i32 + area.width as i32 - width as i32;
    let max_y = area.y as i32 + area.height as i32 - height as i32;
    let x = x.clamp(area.x as i32, max_x.max(area.x as i32));
    let y = y.clamp(area.y as i32, max_y.max(area.y as i32));

    Rect::new(x as u16, y as u16, width, height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centered_overlay_sits_in_the_middle() {
        let area = Rect::new(0, 0, 100, 40);
        let rect = resolve(area, &OverlayOptions::default().width(SizeValue::Cells(50)));
        assert_eq!(rect.width, 50);
        assert_eq!(rect.x, 25);
        assert!(rect.y >= 1 && rect.y + rect.height <= 39);
    }

    #[test]
    fn width_percent_of_terminal() {
        let area = Rect::new(0, 0, 120, 40);
        let rect = resolve(
            area,
            &OverlayOptions::default().width(SizeValue::Percent(50)),
        );
        assert_eq!(rect.width, 60);
    }

    #[test]
    fn respects_min_width() {
        let area = Rect::new(0, 0, 40, 20);
        let rect = resolve(
            area,
            &OverlayOptions::default()
                .width(SizeValue::Cells(5))
                .min_width(20),
        );
        assert_eq!(rect.width, 20);
    }

    #[test]
    fn clamps_inside_terminal() {
        let area = Rect::new(0, 0, 30, 10);
        let rect = resolve(
            area,
            &OverlayOptions::default()
                .width(SizeValue::Percent(90))
                .max_height(SizeValue::Percent(90)),
        );
        assert!(rect.x + rect.width <= area.width);
        assert!(rect.y + rect.height <= area.height);
    }

    #[test]
    fn bottom_right_anchor() {
        let area = Rect::new(0, 0, 100, 40);
        let rect = resolve(
            area,
            &OverlayOptions::default()
                .width(SizeValue::Cells(20))
                .height(SizeValue::Cells(5))
                .min_width(0)
                .anchor(Anchor::BottomRight),
        );
        assert_eq!(rect.width, 20);
        assert_eq!(rect.x, 79);
        assert_eq!(rect.y, 34);
    }

    #[test]
    fn min_width_lifts_small_dialogs() {
        let area = Rect::new(0, 0, 100, 40);
        let rect = resolve(area, &OverlayOptions::default().width(SizeValue::Cells(10)));
        assert_eq!(rect.width, 30);
    }

    #[test]
    fn zero_sized_area_is_safe() {
        let rect = resolve(Rect::new(0, 0, 0, 0), &OverlayOptions::default());
        assert_eq!(rect.width, 0);
        assert_eq!(rect.height, 0);
    }
}
