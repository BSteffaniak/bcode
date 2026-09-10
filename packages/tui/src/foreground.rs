//! TUI-owned foreground identity and background presentation boundary.

use bmux_tui::buffer::Buffer;
use bmux_tui::frame::Frame;
use bmux_tui::geometry::{Point, Rect};
use bmux_tui::paint::{LocalRect, PaintCx};

/// The single foreground receiving modal input and contributing interaction metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Foreground {
    Plugin,
    Sessions,
    AuthPool,
    Model,
    Skill,
    Ralph,
    WorkingDirectory,
    Worktree,
    Permission,
    Streaming,
    Timeline,
    Thinking,
    Theme,
    Commands,
    Slash,
}

impl Foreground {
    /// Slash completion shares the editor rather than taking modal focus.
    pub const fn is_modal(self) -> bool {
        !matches!(self, Self::Slash)
    }
}

/// Paint a modal backdrop as cells only. Covered controls, cursor, semantics,
/// selection and protocol images cannot escape into the foreground's scene.
/// Work and storage are bounded by the terminal canvas, not transcript length.
pub fn paint_backdrop(
    area: Rect,
    target: &mut PaintCx<'_, '_>,
    paint: impl FnOnce(&mut PaintCx<'_, '_>),
) -> Buffer {
    let mut buffer = Buffer::empty(area);
    let mut frame = Frame::new(&mut buffer);
    paint(&mut PaintCx::new(&mut frame));
    paint_backdrop_buffer(&buffer, target);
    buffer
}

/// Reuse a prepared backdrop without invoking background components again.
pub fn paint_backdrop_buffer(buffer: &Buffer, target: &mut PaintCx<'_, '_>) {
    target.rasterize(LocalRect::terminal(buffer.area()), |x, y| {
        let point = Point::new(u16::try_from(x).ok()?, u16::try_from(y).ok()?);
        let cell = buffer.get(point)?;
        (!cell.is_wide_continuation()).then(|| (cell.symbol.clone(), cell.style))
    });
}
