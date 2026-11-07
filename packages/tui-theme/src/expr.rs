//! Bounded palette color expressions.
//!
//! Schema-v2 palette and style values may derive colors with a fixed set of
//! functions evaluated in OKLab/OKLCH by [`bmux_color`]:
//!
//! * `mix(a, b, t)` — `t` is the share of `a`;
//! * `lighten(c, t)`, `darken(c, t)` — move lightness by that fraction of the
//!   remaining distance to white or black;
//! * `saturate(c, t)`, `desaturate(c, t)` — scale chroma by `1 ± t`;
//! * `ensure_contrast(fg, bg, ratio)` — minimal lightness change to reach `ratio`;
//! * `or(a, b, …)` — the first argument whose references all exist.
//!
//! Arguments are palette references (`$name`), direct colors, numbers, or
//! nested calls, nested at most [`MAX_EXPRESSION_DEPTH`] deep. Expressions are
//! data: there are no variables, loops, or side effects.

use bmux_tui::style::Color;

/// Maximum nesting depth of function calls.
pub const MAX_EXPRESSION_DEPTH: usize = 4;
/// Maximum accepted expression length in bytes.
pub const MAX_EXPRESSION_BYTES: usize = 512;

/// Parsed expression.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// Palette reference without the leading `$`.
    Reference(String),
    /// Direct color literal (`#rrggbb`, `ansi:*`, `terminal`, …) kept as source text.
    Literal(String),
    /// Numeric argument.
    Number(f64),
    /// Function call.
    Call(Function, Vec<Self>),
}

/// Supported functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Function {
    /// `mix(a, b, t)`.
    Mix,
    /// `lighten(c, t)`.
    Lighten,
    /// `darken(c, t)`.
    Darken,
    /// `saturate(c, t)`.
    Saturate,
    /// `desaturate(c, t)`.
    Desaturate,
    /// `ensure_contrast(fg, bg, ratio)`.
    EnsureContrast,
    /// `or(a, b, …)`.
    Or,
}

impl Function {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "mix" => Self::Mix,
            "lighten" => Self::Lighten,
            "darken" => Self::Darken,
            "saturate" => Self::Saturate,
            "desaturate" => Self::Desaturate,
            "ensure_contrast" => Self::EnsureContrast,
            "or" => Self::Or,
            _ => return None,
        })
    }

    const fn name(self) -> &'static str {
        match self {
            Self::Mix => "mix",
            Self::Lighten => "lighten",
            Self::Darken => "darken",
            Self::Saturate => "saturate",
            Self::Desaturate => "desaturate",
            Self::EnsureContrast => "ensure_contrast",
            Self::Or => "or",
        }
    }
}

/// Whether `value` looks like an expression rather than a plain color.
#[must_use]
pub fn is_expression(value: &str) -> bool {
    value.contains('(')
}

/// Parse an expression.
///
/// # Errors
///
/// Returns a description of the first syntax error, unknown function, wrong
/// argument count, excessive nesting, or excessive length.
pub fn parse(source: &str) -> Result<Expr, String> {
    if source.len() > MAX_EXPRESSION_BYTES {
        return Err(format!("expression exceeds {MAX_EXPRESSION_BYTES} bytes"));
    }
    let mut parser = Parser {
        source,
        position: 0,
    };
    let expr = parser.expr(0)?;
    parser.skip_space();
    if parser.position != source.len() {
        return Err(format!(
            "unexpected {:?} at byte {}",
            &source[parser.position..],
            parser.position
        ));
    }
    Ok(expr)
}

struct Parser<'a> {
    source: &'a str,
    position: usize,
}

impl Parser<'_> {
    fn rest(&self) -> &str {
        &self.source[self.position..]
    }

    fn skip_space(&mut self) {
        let trimmed = self.rest().trim_start();
        self.position = self.source.len() - trimmed.len();
    }

    fn token(&mut self) -> &str {
        self.skip_space();
        let rest = self.rest();
        let end = rest
            .find(|character: char| {
                matches!(character, '(' | ')' | ',') || character.is_whitespace()
            })
            .unwrap_or(rest.len());
        let token = &self.source[self.position..self.position + end];
        self.position += end;
        token
    }

    fn eat(&mut self, expected: char) -> bool {
        self.skip_space();
        if self.rest().starts_with(expected) {
            self.position += expected.len_utf8();
            true
        } else {
            false
        }
    }

    fn expr(&mut self, depth: usize) -> Result<Expr, String> {
        let token = self.token().to_owned();
        if token.is_empty() {
            return Err(format!("expected a value at byte {}", self.position));
        }
        if self.eat('(') {
            let function =
                Function::parse(&token).ok_or_else(|| format!("unknown function {token:?}"))?;
            if depth >= MAX_EXPRESSION_DEPTH {
                return Err(format!(
                    "expressions may nest at most {MAX_EXPRESSION_DEPTH} calls"
                ));
            }
            let mut arguments = vec![self.expr(depth + 1)?];
            while self.eat(',') {
                arguments.push(self.expr(depth + 1)?);
            }
            if !self.eat(')') {
                return Err(format!("expected ')' after {}(…)", function.name()));
            }
            // The final argument of every color function is its amount, so an
            // integer there is a number rather than a palette index.
            if function != Function::Or
                && let Some(last) = arguments.last_mut()
                && let Expr::Literal(token) = last
                && let Ok(number) = token.parse::<f64>()
                && number.is_finite()
            {
                *last = Expr::Number(number);
            }
            check_arity(function, &arguments)?;
            return Ok(Expr::Call(function, arguments));
        }
        if let Some(reference) = token.strip_prefix('$') {
            return Ok(Expr::Reference(reference.to_owned()));
        }
        if let Ok(number) = token.parse::<f64>()
            && number.is_finite()
            && !token.chars().all(|character| character.is_ascii_digit())
        {
            return Ok(Expr::Number(number));
        }
        Ok(Expr::Literal(token))
    }
}

fn check_arity(function: Function, arguments: &[Expr]) -> Result<(), String> {
    let expected = match function {
        Function::Mix | Function::EnsureContrast => 3,
        Function::Lighten | Function::Darken | Function::Saturate | Function::Desaturate => 2,
        Function::Or => {
            return if arguments.len() >= 2 {
                Ok(())
            } else {
                Err("or(…) needs at least two arguments".to_owned())
            };
        }
    };
    if arguments.len() == expected {
        Ok(())
    } else {
        Err(format!(
            "{}(…) takes {expected} arguments, found {}",
            function.name(),
            arguments.len()
        ))
    }
}

/// Result of evaluating an expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evaluated {
    /// Exact color.
    Exact(Color),
    /// Math was applied to a terminal-owned (non-RGB) color; the first color
    /// argument was used unchanged.
    Approximate(Color, String),
    /// A reference was missing (only meaningful inside `or`).
    Missing(String),
}

/// Evaluation context resolving references and literals.
pub trait ColorContext {
    /// Resolve a palette reference, or `None` when it does not exist.
    ///
    /// # Errors
    ///
    /// Returns a message when the reference exists but cannot be resolved.
    fn reference(&mut self, name: &str) -> Result<Option<Color>, String>;

    /// Parse a direct color literal.
    ///
    /// # Errors
    ///
    /// Returns a message for malformed literals.
    fn literal(&mut self, value: &str) -> Result<Color, String>;
}

/// Evaluate `expr`.
///
/// # Errors
///
/// Returns a message for missing references (outside `or`), malformed
/// literals, numbers used as colors, or colors used as numbers.
pub fn evaluate(expr: &Expr, context: &mut impl ColorContext) -> Result<Evaluated, String> {
    match expr {
        Expr::Reference(name) => Ok(context
            .reference(name)?
            .map_or_else(|| Evaluated::Missing(name.clone()), Evaluated::Exact)),
        Expr::Literal(value) => context.literal(value).map(Evaluated::Exact),
        Expr::Number(number) => Err(format!("number {number} used where a color is expected")),
        Expr::Call(Function::Or, arguments) => {
            let mut missing = Vec::new();
            for argument in arguments {
                match evaluate(argument, context)? {
                    Evaluated::Missing(name) => missing.push(name),
                    found => return Ok(found),
                }
            }
            Ok(Evaluated::Missing(missing.join(", ")))
        }
        Expr::Call(function, arguments) => {
            let mut colors = Vec::new();
            let mut amount = None;
            let mut note = None;
            for argument in arguments {
                if let Expr::Number(number) = argument {
                    amount = Some(*number);
                    continue;
                }
                match evaluate(argument, context)? {
                    Evaluated::Exact(color) => colors.push(color),
                    Evaluated::Approximate(color, reason) => {
                        note.get_or_insert(reason);
                        colors.push(color);
                    }
                    missing @ Evaluated::Missing(_) => return Ok(missing),
                }
            }
            let amount = amount.ok_or_else(|| format!("{}(…) needs a number", function.name()))?;
            apply(*function, &colors, amount, note)
        }
    }
}

fn apply(
    function: Function,
    colors: &[Color],
    amount: f64,
    note: Option<String>,
) -> Result<Evaluated, String> {
    let expected = if matches!(function, Function::Mix | Function::EnsureContrast) {
        2
    } else {
        1
    };
    if colors.len() != expected {
        return Err(format!(
            "{}(…) takes {expected} colors and one number",
            function.name()
        ));
    }
    let rgb: Option<Vec<bmux_color::Rgb>> = colors.iter().map(|color| color.rgb()).collect();
    let Some(rgb) = rgb else {
        let reason = format!(
            "{}(…) applied to a terminal-owned color; using the first color unchanged",
            function.name()
        );
        return Ok(Evaluated::Approximate(colors[0], reason));
    };
    let result = match function {
        Function::Mix => bmux_color::mix(rgb[0], rgb[1], amount),
        Function::Lighten => bmux_color::lighten(rgb[0], amount),
        Function::Darken => bmux_color::darken(rgb[0], amount),
        Function::Saturate => bmux_color::saturate(rgb[0], amount),
        Function::Desaturate => bmux_color::desaturate(rgb[0], amount),
        Function::EnsureContrast => bmux_color::ensure_contrast(rgb[0], rgb[1], amount),
        Function::Or => unreachable!("or is evaluated before apply"),
    };
    let color = Color::from(result);
    Ok(note.map_or(Evaluated::Exact(color), |note| {
        Evaluated::Approximate(color, note)
    }))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    struct Palette(BTreeMap<&'static str, Color>);

    impl ColorContext for Palette {
        fn reference(&mut self, name: &str) -> Result<Option<Color>, String> {
            Ok(self.0.get(name).copied())
        }

        fn literal(&mut self, value: &str) -> Result<Color, String> {
            match value {
                "terminal" => Ok(Color::Default),
                "ansi:green" => Ok(Color::Green),
                hex => bmux_color::Rgb::from_hex(hex)
                    .map(Color::from)
                    .map_err(|error| error.to_string()),
            }
        }
    }

    fn palette() -> Palette {
        Palette(BTreeMap::from([
            ("bg", Color::Rgb(0, 0, 0)),
            ("fg", Color::Rgb(255, 255, 255)),
            ("green", Color::Rgb(0x40, 0xa0, 0x2b)),
            ("ansi", Color::Green),
        ]))
    }

    fn eval(source: &str) -> Result<Evaluated, String> {
        evaluate(&parse(source)?, &mut palette())
    }

    #[test]
    fn evaluates_math_through_bmux_color() {
        assert_eq!(
            eval("mix($fg, $bg, 1.0)"),
            Ok(Evaluated::Exact(Color::Rgb(255, 255, 255)))
        );
        assert_eq!(
            eval("lighten($bg, 1.0)"),
            Ok(Evaluated::Exact(Color::Rgb(255, 255, 255)))
        );
        let Ok(Evaluated::Exact(Color::Rgb(r, g, b))) =
            eval("ensure_contrast(mix($green, $bg, 0.2), $bg, 4.5)")
        else {
            panic!("exact rgb");
        };
        assert!(bmux_color::Rgb::new(r, g, b).contrast_ratio(bmux_color::Rgb::BLACK) >= 4.5);
        assert!(matches!(
            eval("darken(#ffffff, 0.5)"),
            Ok(Evaluated::Exact(Color::Rgb(..)))
        ));
    }

    #[test]
    fn terminal_owned_inputs_degrade_with_a_note() {
        let Ok(Evaluated::Approximate(color, note)) = eval("mix($ansi, $bg, 0.5)") else {
            panic!("approximate");
        };
        assert_eq!(color, Color::Green);
        assert!(note.contains("terminal-owned"));
    }

    #[test]
    fn or_picks_the_first_available_argument() {
        assert_eq!(
            eval("or($violet, $magenta, $green)"),
            Ok(Evaluated::Exact(Color::Rgb(0x40, 0xa0, 0x2b)))
        );
        assert_eq!(
            eval("or($violet, $magenta)"),
            Ok(Evaluated::Missing("violet, magenta".to_owned()))
        );
        assert_eq!(
            eval("mix($violet, $bg, 0.5)"),
            Ok(Evaluated::Missing("violet".to_owned()))
        );
    }

    #[test]
    fn rejects_malformed_expressions() {
        for (source, fragment) in [
            ("mix($fg, $bg)", "takes 3 arguments"),
            ("blend($fg, $bg, 0.5)", "unknown function"),
            ("mix($fg, $bg, 0.5", "expected ')'"),
            ("mix($fg, $bg, 0.5) extra", "unexpected"),
            ("or($fg)", "at least two"),
            (
                "lighten(lighten(lighten(lighten(lighten($fg, 0.1), 0.1), 0.1), 0.1), 0.1)",
                "nest at most",
            ),
        ] {
            let error = parse(source).expect_err(source);
            assert!(error.contains(fragment), "{source}: {error}");
        }
        assert!(parse(&"x".repeat(MAX_EXPRESSION_BYTES + 1)).is_err());
        assert!(
            eval("mix($fg, $bg, $bg)")
                .unwrap_err()
                .contains("needs a number")
        );
        assert!(eval("lighten(0.5, 0.5)").unwrap_err().contains("number"));
    }

    #[test]
    fn integers_are_amounts_only_in_amount_position() {
        // Bare integers remain palette indices in color positions.
        assert_eq!(parse("244"), Ok(Expr::Literal("244".to_owned())));
        assert_eq!(parse("0.5"), Ok(Expr::Number(0.5)));
        assert_eq!(
            parse("ensure_contrast($a, $b, 3)"),
            Ok(Expr::Call(
                Function::EnsureContrast,
                vec![
                    Expr::Reference("a".to_owned()),
                    Expr::Reference("b".to_owned()),
                    Expr::Number(3.0)
                ]
            ))
        );
    }
}
