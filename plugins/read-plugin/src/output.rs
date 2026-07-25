use std::io::{self, Write};

use bmux_tui::prelude::{Color, Line, Modifier, Style};
use crossterm::QueueableCommand as _;
use crossterm::style::{
    Attribute, Color as CrosstermColor, ResetColor, SetAttribute, SetBackgroundColor,
    SetForegroundColor,
};

pub(crate) fn write_lines(writer: &mut impl Write, lines: &[Line], styled: bool) -> io::Result<()> {
    let result = if styled {
        write_styled_lines(writer, lines)
    } else {
        write_plain_lines(writer, lines)
    };
    match result {
        Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(()),
        result => result,
    }
}

fn write_plain_lines(writer: &mut impl Write, lines: &[Line]) -> io::Result<()> {
    for line in lines {
        for span in &line.spans {
            writer.write_all(span.content.as_bytes())?;
        }
        writer.write_all(b"\n")?;
    }
    writer.flush()
}

fn write_styled_lines(writer: &mut impl Write, lines: &[Line]) -> io::Result<()> {
    for line in lines {
        let mut active_style = Style::new();
        for span in &line.spans {
            if span.style != active_style {
                queue_style(writer, span.style)?;
                active_style = span.style;
            }
            writer.write_all(span.content.as_bytes())?;
        }
        if active_style != Style::new() {
            queue_reset(writer)?;
        }
        writer.write_all(b"\n")?;
    }
    writer.flush()
}

fn queue_style(writer: &mut impl Write, style: Style) -> io::Result<()> {
    queue_reset(writer)?;
    if let Some(foreground) = style.fg {
        writer.queue(SetForegroundColor(to_crossterm_color(foreground)))?;
    }
    if let Some(background) = style.bg {
        writer.queue(SetBackgroundColor(to_crossterm_color(background)))?;
    }
    for (modifier, attribute) in [
        (Modifier::BOLD, Attribute::Bold),
        (Modifier::DIM, Attribute::Dim),
        (Modifier::ITALIC, Attribute::Italic),
        (Modifier::UNDERLINE, Attribute::Underlined),
        (Modifier::SLOW_BLINK, Attribute::SlowBlink),
        (Modifier::REVERSED, Attribute::Reverse),
        (Modifier::HIDDEN, Attribute::Hidden),
        (Modifier::CROSSED_OUT, Attribute::CrossedOut),
    ] {
        if style.modifiers.contains(modifier) {
            writer.queue(SetAttribute(attribute))?;
        }
    }
    Ok(())
}

fn queue_reset(writer: &mut impl Write) -> io::Result<()> {
    writer.queue(SetAttribute(Attribute::Reset))?;
    writer.queue(ResetColor)?;
    Ok(())
}

const fn to_crossterm_color(color: Color) -> CrosstermColor {
    match color {
        Color::Default => CrosstermColor::Reset,
        Color::Black => CrosstermColor::Black,
        Color::Red => CrosstermColor::DarkRed,
        Color::Green => CrosstermColor::DarkGreen,
        Color::Yellow => CrosstermColor::DarkYellow,
        Color::Blue => CrosstermColor::DarkBlue,
        Color::Magenta => CrosstermColor::DarkMagenta,
        Color::Cyan => CrosstermColor::DarkCyan,
        Color::White => CrosstermColor::Grey,
        Color::BrightBlack => CrosstermColor::DarkGrey,
        Color::BrightRed => CrosstermColor::Red,
        Color::BrightGreen => CrosstermColor::Green,
        Color::BrightYellow => CrosstermColor::Yellow,
        Color::BrightBlue => CrosstermColor::Blue,
        Color::BrightMagenta => CrosstermColor::Magenta,
        Color::BrightCyan => CrosstermColor::Cyan,
        Color::BrightWhite => CrosstermColor::White,
        Color::Indexed(index) => CrosstermColor::AnsiValue(index),
        Color::Rgb(red, green, blue) => CrosstermColor::Rgb {
            r: red,
            g: green,
            b: blue,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bmux_tui::prelude::Span;

    #[test]
    fn plain_output_preserves_layout_without_escape_sequences() {
        let lines = vec![
            Line::from_spans(vec![
                Span::styled("hello", Style::new().fg(Color::Cyan)),
                Span::raw(" world"),
            ]),
            Line::new(),
        ];
        let mut output = Vec::new();

        write_lines(&mut output, &lines, false).unwrap();

        assert_eq!(output, b"hello world\n\n");
        assert!(!output.contains(&0x1b));
    }

    #[test]
    fn styled_output_maps_colors_modifiers_and_resets() {
        let style = Style::new()
            .fg(Color::Rgb(1, 2, 3))
            .bg(Color::Indexed(4))
            .add_modifier(Modifier::BOLD | Modifier::ITALIC | Modifier::UNDERLINE);
        let lines = vec![Line::from_spans(vec![Span::styled("hello", style)])];
        let mut output = Vec::new();

        write_lines(&mut output, &lines, true).unwrap();
        let output = String::from_utf8(output).unwrap();

        assert!(output.contains("\u{1b}[38;2;1;2;3m"));
        assert!(output.contains("\u{1b}[48;5;4m"));
        assert!(output.contains("\u{1b}[1m"));
        assert!(output.contains("\u{1b}[3m"));
        assert!(output.contains("\u{1b}[4m"));
        assert!(output.ends_with("\u{1b}[0m\u{1b}[0m\n"));
    }

    #[test]
    fn every_color_maps_to_an_ansi_sequence() {
        let colors = [
            Color::Default,
            Color::Black,
            Color::Red,
            Color::Green,
            Color::Yellow,
            Color::Blue,
            Color::Magenta,
            Color::Cyan,
            Color::White,
            Color::BrightBlack,
            Color::BrightRed,
            Color::BrightGreen,
            Color::BrightYellow,
            Color::BrightBlue,
            Color::BrightMagenta,
            Color::BrightCyan,
            Color::BrightWhite,
            Color::Indexed(123),
            Color::Rgb(1, 2, 3),
        ];
        for color in colors {
            let mut output = Vec::new();
            write_lines(
                &mut output,
                &[Line::from_spans(vec![Span::styled(
                    "x",
                    Style::new().fg(color).bg(color),
                )])],
                true,
            )
            .unwrap();
            assert!(output.contains(&0x1b), "missing ANSI for {color:?}");
            assert!(output.contains(&b'x'));
        }
    }

    #[test]
    fn every_modifier_maps_to_an_ansi_sequence() {
        for modifier in [
            Modifier::BOLD,
            Modifier::DIM,
            Modifier::ITALIC,
            Modifier::UNDERLINE,
            Modifier::SLOW_BLINK,
            Modifier::REVERSED,
            Modifier::HIDDEN,
            Modifier::CROSSED_OUT,
        ] {
            let mut output = Vec::new();
            write_lines(
                &mut output,
                &[Line::from_spans(vec![Span::styled(
                    "x",
                    Style::new().add_modifier(modifier),
                )])],
                true,
            )
            .unwrap();
            assert!(output.contains(&0x1b), "missing ANSI for {modifier:?}");
            assert!(output.contains(&b'x'));
        }
    }

    struct FailingWriter {
        kind: io::ErrorKind,
    }

    impl Write for FailingWriter {
        fn write(&mut self, _buffer: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(self.kind, "closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn broken_pipe_is_success() {
        for styled in [true, false] {
            write_lines(
                &mut FailingWriter {
                    kind: io::ErrorKind::BrokenPipe,
                },
                &[Line::raw("hello")],
                styled,
            )
            .unwrap();
        }
    }

    #[test]
    fn non_broken_writer_errors_are_preserved() {
        for styled in [true, false] {
            let error = write_lines(
                &mut FailingWriter {
                    kind: io::ErrorKind::PermissionDenied,
                },
                &[Line::raw("hello")],
                styled,
            )
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        }
    }
}
