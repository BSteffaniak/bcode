//! Parser-owned symbolic argv shapes; no expansion or environment reads occur here.

use bcode_shell_command_analysis_models::ShellArgumentShape;
use brush_parser::word::{
    Parameter, ParameterExpr, SpecialParameter, WordPiece, WordPieceWithSource,
};

pub fn shape(pieces: &[WordPieceWithSource]) -> ShellArgumentShape {
    let mut prefix = String::new();
    let mut suffix = String::new();
    let mut unknown = false;
    if one(pieces, false, &mut prefix, &mut suffix, &mut unknown) {
        ShellArgumentShape::One { prefix, suffix }
    } else {
        ShellArgumentShape::Many
    }
}

fn one(
    pieces: &[WordPieceWithSource],
    quoted: bool,
    prefix: &mut String,
    suffix: &mut String,
    unknown: &mut bool,
) -> bool {
    for piece in pieces {
        let text = match &piece.piece {
            WordPiece::Text(text) if quoted || !text.contains(['*', '?', '[']) => {
                Some(text.clone())
            }
            WordPiece::SingleQuotedText(text) => Some(text.clone()),
            WordPiece::EscapeSequence(text) => Some(brush_parser::unquote_str(text)),
            WordPiece::DoubleQuotedSequence(nested) => {
                if !one(nested, true, prefix, suffix, unknown) {
                    return false;
                }
                continue;
            }
            WordPiece::ParameterExpansion(ParameterExpr::Parameter {
                parameter,
                indirect: false,
            }) if quoted && scalar(parameter) => None,
            WordPiece::CommandSubstitution(_) | WordPiece::BackquotedCommandSubstitution(_)
                if quoted =>
            {
                None
            }
            _ => return false,
        };
        if let Some(text) = text {
            if *unknown {
                suffix.push_str(&text);
            } else {
                prefix.push_str(&text);
            }
        } else {
            *unknown = true;
            suffix.clear();
        }
    }
    true
}

const fn scalar(parameter: &Parameter) -> bool {
    matches!(
        parameter,
        Parameter::Named(_)
            | Parameter::Positional(_)
            | Parameter::Special(
                SpecialParameter::PositionalParameterCount
                    | SpecialParameter::LastExitStatus
                    | SpecialParameter::CurrentOptionFlags
                    | SpecialParameter::ProcessId
                    | SpecialParameter::LastBackgroundProcessId
                    | SpecialParameter::ShellName
                    | SpecialParameter::AllPositionalParameters { concatenate: true }
            )
    )
}

/// Return the recursively expandable operand of supported POSIX scalar parameter operators.
/// Errors mean unsupported syntax; `Ok(None)` means no operand.
pub fn parameter_operand(expr: &ParameterExpr) -> Result<Option<&str>, ()> {
    let (parameter, operand) = match expr {
        ParameterExpr::UseDefaultValues {
            parameter,
            indirect: false,
            default_value,
            ..
        }
        | ParameterExpr::AssignDefaultValues {
            parameter,
            indirect: false,
            default_value,
            ..
        } => (parameter, default_value.as_deref()),
        ParameterExpr::UseAlternativeValue {
            parameter,
            indirect: false,
            alternative_value,
            ..
        } => (parameter, alternative_value.as_deref()),
        ParameterExpr::IndicateErrorIfNullOrUnset {
            parameter,
            indirect: false,
            error_message,
            ..
        } => (parameter, error_message.as_deref()),
        ParameterExpr::RemoveSmallestSuffixPattern {
            parameter,
            indirect: false,
            pattern,
        }
        | ParameterExpr::RemoveLargestSuffixPattern {
            parameter,
            indirect: false,
            pattern,
        }
        | ParameterExpr::RemoveSmallestPrefixPattern {
            parameter,
            indirect: false,
            pattern,
        }
        | ParameterExpr::RemoveLargestPrefixPattern {
            parameter,
            indirect: false,
            pattern,
        } => (parameter, pattern.as_deref()),
        ParameterExpr::ParameterLength {
            parameter,
            indirect: false,
        } => (parameter, None),
        _ => return Err(()),
    };
    scalar(parameter).then_some(operand).ok_or(())
}

pub fn operands<'a>(pieces: &'a [WordPieceWithSource], out: &mut Vec<&'a str>) {
    for piece in pieces {
        match &piece.piece {
            WordPiece::ParameterExpansion(expr) => {
                if let Ok(Some(operand)) = parameter_operand(expr) {
                    out.push(operand);
                }
            }
            WordPiece::DoubleQuotedSequence(nested)
            | WordPiece::GettextDoubleQuotedSequence(nested) => operands(nested, out),
            _ => {}
        }
    }
}

/// Complex parameter/arithmetic expansions can contain executable source. Until recursively
/// analyzed, report them as unsupported rather than granting broad-policy authorization.
pub fn unsupported(pieces: &[WordPieceWithSource]) -> bool {
    pieces.iter().any(|piece| match &piece.piece {
        WordPiece::ParameterExpansion(ParameterExpr::Parameter {
            parameter,
            indirect: false,
        }) => {
            !scalar(parameter)
                && !matches!(
                    parameter,
                    Parameter::Special(SpecialParameter::AllPositionalParameters { .. })
                )
        }
        WordPiece::ParameterExpansion(expr) if parameter_operand(expr).is_ok() => false,
        WordPiece::ParameterExpansion(_) | WordPiece::ArithmeticExpression(_) => true,
        WordPiece::DoubleQuotedSequence(nested)
        | WordPiece::GettextDoubleQuotedSequence(nested) => unsupported(nested),
        _ => false,
    })
}
