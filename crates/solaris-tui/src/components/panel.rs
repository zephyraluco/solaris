//! Bordered container that draws a child inside its frame.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, BorderType, Borders, Widget};

use crate::component::Component;

/// A bordered box with an optional title and child.
pub struct Panel {
    title: String,
    border_style: Style,
    inner_style: Style,
    border_type: BorderType,
    content: Option<Box<dyn Component>>,
}

impl Panel {
    /// An empty bordered panel.
    pub fn new() -> Self {
        Self {
            title: String::new(),
            border_style: Style::default(),
            inner_style: Style::default(),
            border_type: BorderType::Rounded,
            content: None,
        }
    }

    /// Set the title shown in the top border.
    pub fn title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Style the border.
    pub fn border_style(mut self, style: Style) -> Self {
        self.border_style = style;
        self
    }

    /// Fill the interior with a background style.
    pub fn inner_style(mut self, style: Style) -> Self {
        self.inner_style = style;
        self
    }

    /// Use a different border glyph set.
    pub fn border_type(mut self, border_type: BorderType) -> Self {
        self.border_type = border_type;
        self
    }

    /// Put a child inside the frame.
    pub fn content(mut self, content: Box<dyn Component>) -> Self {
        self.content = Some(content);
        self
    }

    /// Replace the child.
    pub fn set_content(&mut self, content: Box<dyn Component>) {
        self.content = Some(content);
    }

    /// The area available to the child.
    pub fn inner_area(&self, area: Rect) -> Rect {
        block(&self.title, self.border_type).inner(area)
    }
}

impl Default for Panel {
    fn default() -> Self {
        Self::new()
    }
}

fn block(title: &str, border_type: BorderType) -> Block<'static> {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(border_type);
    if title.is_empty() {
        block
    } else {
        block.title(format!(" {title} "))
    }
}

impl Component for Panel {
    fn render(&mut self, buf: &mut Buffer, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let block = block(&self.title, self.border_type).border_style(self.border_style);
        let inner = block.inner(area);
        block.render(area, buf);

        if self.inner_style != Style::default() {
            buf.set_style(inner, self.inner_style);
        }
        if let Some(content) = self.content.as_mut() {
            content.render(buf, inner);
        }
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> crate::component::KeyResult {
        match self.content.as_mut() {
            Some(content) => content.handle_key(key),
            None => crate::component::KeyResult::Ignored,
        }
    }

    fn handle_mouse(
        &mut self,
        event: crossterm::event::MouseEvent,
    ) -> crate::component::MouseResult {
        match self.content.as_mut() {
            Some(content) => content.handle_mouse(event),
            None => crate::component::MouseResult::Ignored,
        }
    }

    fn handle_paste(&mut self, text: &str) -> crate::component::KeyResult {
        match self.content.as_mut() {
            Some(content) => content.handle_paste(text),
            None => crate::component::KeyResult::Ignored,
        }
    }

    fn tick(&mut self) -> bool {
        self.content.as_mut().is_some_and(|content| content.tick())
    }

    fn desired_height(&mut self, width: u16) -> Option<u16> {
        let inner_width = width.saturating_sub(2);
        let inner = self.desired_inner_height(inner_width)?;
        Some(inner.saturating_add(2))
    }

    fn version(&self) -> u64 {
        self.content.as_ref().map_or(0, |c| c.version())
    }
}

impl Panel {
    fn desired_inner_height(&mut self, width: u16) -> Option<u16> {
        self.content
            .as_mut()
            .and_then(|content| content.desired_height(width))
    }
}
