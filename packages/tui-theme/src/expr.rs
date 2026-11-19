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
//! * `readable(fg, bg, ratio)` — `ensure_contrast` for computed colors, while a
//!   color taken directly from the palette passes through unchanged (see
//!   [`palette_source`]) unless the context adjusts authored colors;
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
    /// `readable(fg, bg, ratio)`.
    Readable,
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
            "readable" => Self::Readable,
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
            Self::Readable => "readable",
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
        Function::Mix | Function::EnsureContrast | Function::Readable => 3,
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

    /// Whether `readable(…)` also raises colors taken directly from the
    /// palette. By default palette colors pass through unchanged.
    fn adjusts_palette_colors(&self) -> bool {
        false
    }
}

/// The palette entry `expr` takes its value from unchanged, if any.
///
/// A bare reference names its entry; `or(…)` follows its first available
/// argument; `readable(…)` follows its color argument unless `context`
/// adjusts palette colors. Every other function computes a new color, so its
/// result has no palette source. A missing reference has none either.
///
/// # Errors
///
/// Returns a message when evaluating an `or(…)` argument fails.
pub fn palette_source(
    expr: &Expr,
    context: &mut impl ColorContext,
) -> Result<Option<String>, String> {
    match expr {
        Expr::Reference(name) => Ok(context.reference(name)?.map(|_| name.clone())),
        Expr::Call(Function::Or, arguments) => {
            for argument in arguments {
                if !matches!(evaluate(argument, context)?, Evaluated::Missing(_)) {
                    return palette_source(argument, context);
                }
            }
            Ok(None)
        }
        Expr::Call(Function::Readable, arguments) if !context.adjusts_palette_colors() => {
            palette_source(&arguments[0], context)
        }
        _ => Ok(None),
    }
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
        Expr::Call(Function::Readable, arguments) if palette_source(expr, context)?.is_some() => {
            // A palette color passes through exactly; terminal-owned colors
            // fall through so the math reports them as approximate.
            match evaluate(&arguments[0], context)? {
                Evaluated::Exact(color @ Color::Rgb(..)) => Ok(Evaluated::Exact(color)),
                _ => evaluate_math(Function::Readable, arguments, context),
            }
        }
        Expr::Call(function, arguments) => evaluate_math(*function, arguments, context),
    }
}

fn evaluate_math(
    function: Function,
    arguments: &[Expr],
    context: &mut impl ColorContext,
) -> Result<Evaluated, String> {
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
    apply(function, &colors, amount, note)
}

fn apply(
    function: Function,
    colors: &[Color],
    amount: f64,
    note: Option<String>,
) -> Result<Evaluated, String> {
    let expected = if matches!(
        function,
        Function::Mix | Function::EnsureContrast | Function::Readable
    ) {
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
        Function::EnsureContrast | Function::Readable => {
            bmux_color::ensure_contrast(rgb[0], rgb[1], amount)
        }
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

    struct Palette(BTreeMap<&'static str, Color>, bool);

    impl ColorContext for Palette {
        fn reference(&mut self, name: &str) -> Result<Option<Color>, String> {
            Ok(self.0.get(name).copied())
        }

        fn adjusts_palette_colors(&self) -> bool {
            self.1
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
        Palette(
            BTreeMap::from([
                ("bg", Color::Rgb(0, 0, 0)),
                ("fg", Color::Rgb(255, 255, 255)),
                ("green", Color::Rgb(0x40, 0xa0, 0x2b)),
                ("ansi", Color::Green),
                ("dim", Color::Rgb(0x20, 0x20, 0x20)),
            ]),
            false,
        )
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
    fn readable_keeps_palette_colors_and_raises_computed_ones() {
        let dim = Evaluated::Exact(Color::Rgb(0x20, 0x20, 0x20));
        assert_eq!(eval("readable($dim, $bg, 4.5)"), Ok(dim.clone()));
        assert_eq!(eval("readable(or($missing, $dim), $bg, 4.5)"), Ok(dim));
        let source = |source: &str, palette: &mut Palette| {
            palette_source(&parse(source).expect("parses"), palette)
        };
        assert_eq!(
            source("readable(or($missing, $dim), $bg, 4.5)", &mut palette()),
            Ok(Some("dim".to_owned()))
        );
        assert_eq!(
            source("or($missing, mix($dim, $bg, 0.5))", &mut palette()),
            Ok(None)
        );
        for computed in [
            "readable(mix($dim, $bg, 0.9), $bg, 4.5)",
            "readable(or($missing, mix($dim, $bg, 0.9)), $bg, 4.5)",
        ] {
            let Ok(Evaluated::Exact(Color::Rgb(r, g, b))) = eval(computed) else {
                panic!("{computed}");
            };
            let raised = bmux_color::Rgb::new(r, g, b);
            assert!(
                raised.contrast_ratio(bmux_color::Rgb::BLACK) >= 4.5,
                "{computed}"
            );
        }
        let mut adjusting = palette();
        adjusting.1 = true;
        assert_eq!(source("readable($dim, $bg, 4.5)", &mut adjusting), Ok(None));
        let Ok(Evaluated::Exact(Color::Rgb(r, g, b))) = evaluate(
            &parse("readable($dim, $bg, 4.5)").expect("parses"),
            &mut adjusting,
        ) else {
            panic!("exact rgb");
        };
        assert!(bmux_color::Rgb::new(r, g, b).contrast_ratio(bmux_color::Rgb::BLACK) >= 4.5);
        // Terminal-owned colors are never passed off as exact.
        assert!(matches!(
            eval("readable($ansi, $bg, 4.5)"),
            Ok(Evaluated::Approximate(Color::Green, _))
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
