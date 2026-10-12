//! Renderer-neutral, source-backed command presentation.
//!
//! Display fragments are not execution or clipboard authority. Original command bytes stay in
//! source zero; decoded embedded programs are separate sources with explicit encoded provenance.

use std::ops::Range;

/// Identity of a retained source within one document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceId(pub usize);

/// Where a retained source came from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SourceOrigin {
    /// Exact command bytes supplied by the caller.
    Original,
    /// A decoded program, not byte-for-byte interchangeable with its parent spelling.
    Decoded {
        /// Source containing the encoded spelling.
        parent: SourceId,
        /// UTF-8 byte range containing that spelling (including delimiters when appropriate).
        encoded: Range<usize>,
    },
}

/// One decoded Unicode scalar and its exact responsible parent bytes.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Correspondence {
    /// UTF-8 byte range of one scalar in the decoded source.
    pub decoded: Range<usize>,
    /// Exact encoded parent bytes responsible for that scalar (including escapes).
    pub original: Range<usize>,
}

/// Retained immutable-by-convention source text.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Source {
    /// Exact source bytes.
    pub text: String,
    /// Original or decoded provenance.
    pub origin: SourceOrigin,
    /// Certain per-scalar mapping into the immediate parent; empty means unavailable.
    pub correspondence: Vec<Correspondence>,
}

/// Provenance of a displayed fragment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Provenance {
    /// Exact bytes from one retained source.
    Source {
        /// Retained source identity.
        id: SourceId,
        /// UTF-8 byte range within that source.
        range: Range<usize>,
    },
    /// Presentation-only whitespace; never executable or independently selectable.
    Generated,
}

/// Semantic styling role, independent of terminal themes or geometry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Role {
    /// Uncertain or unclassified source.
    Literal,
    /// Command name.
    Command,
    /// Statically spelled shell option.
    Flag,
    /// Parser-recognized variable expansion.
    Variable,
    /// Numeric literal.
    Number,
    /// Embedded-language identifier.
    Identifier,
    /// Embedded-language function name or direct call target.
    Function,
    /// Grammar-recognized control keyword.
    Keyword,
    /// Shell punctuation or operator.
    Operator,
    /// Quoted source word.
    String,
    /// Source comment.
    Comment,
    /// Original or generated whitespace.
    Whitespace,
}

/// One cohesive display fragment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Fragment {
    /// Displayed bytes.
    pub text: String,
    /// Renderer-neutral semantic role.
    pub role: Role,
    /// Exact source range or generated whitespace.
    pub provenance: Provenance,
}

/// A logical line, before renderer-owned wrapping and clipping.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Line {
    /// Fragments without their terminating newline.
    pub fragments: Vec<Fragment>,
    /// Provenance of the terminating newline, if present.
    pub newline: Option<Provenance>,
}

/// Presentation input policy; never changes execution bytes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InputPolicy {
    /// Display original bytes without whitespace formatting or embedded decoding;
    /// source-validated syntax roles remain available.
    Literal,
    /// Allow source-backed structural presentation.
    Format,
}

/// Explicit bounded work policy shared by shell and embedded-language adapters.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Budget {
    /// Maximum bytes admitted to a parser in one adapter invocation.
    pub max_source_bytes: usize,
    /// Maximum fragments in a formatted document (literal fallback may use one fragment).
    pub max_fragments: usize,
    /// Maximum shell/embedded language composition depth (not syntax-tree depth).
    /// Adapters independently bound parser syntax and AST visits using byte/fragment budgets.
    pub max_depth: usize,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_source_bytes: 64 * 1024,
            max_fragments: 8192,
            max_depth: 32,
        }
    }
}

/// Explicit source slice and recursion depth supplied to a language adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdapterInput {
    /// Source to present.
    pub source: SourceId,
    /// Exact source byte range.
    pub range: Range<usize>,
    /// Current nesting depth; child invocations increment it.
    pub depth: usize,
}

/// Parser-backed optional language presentation.
///
/// A successful adapter appends fragments; failure must leave the document unchanged. Adapters
/// must retain uncertain source literally, validate source coverage independently of recovered
/// ASTs, and obey the supplied budgets. Callers choose adapters and enforce opt-out policy.
pub trait LanguageAdapter {
    /// Append a presentation of the input, returning false when admission/recognition fails.
    fn present(&self, document: &mut Document, input: &AdapterInput, budget: &Budget) -> bool;
}

/// Source authority plus a renderer-neutral display projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Document {
    /// Retained original and decoded sources; source zero is always the original command.
    pub sources: Vec<Source>,
    /// Ordered displayed fragments.
    pub fragments: Vec<Fragment>,
}

impl Document {
    /// Create an original command document with an initially empty projection.
    #[must_use]
    pub fn original(source: &str) -> Self {
        Self {
            sources: vec![Source {
                text: source.to_owned(),
                origin: SourceOrigin::Original,
                correspondence: Vec::new(),
            }],
            fragments: Vec::new(),
        }
    }

    /// Exact original command bytes, for execution/copy authority.
    #[must_use]
    pub fn execution_text(&self) -> &str {
        self.sources
            .first()
            .map_or("", |source| source.text.as_str())
    }

    /// Concatenate display fragments. This is not execution/copy authority.
    #[must_use]
    pub fn display_text(&self) -> String {
        self.fragments
            .iter()
            .map(|fragment| fragment.text.as_str())
            .collect()
    }

    /// Retain a decoded source only when its parent range is valid.
    pub fn add_decoded(
        &mut self,
        text: String,
        parent: SourceId,
        encoded: Range<usize>,
    ) -> Option<SourceId> {
        self.add_decoded_mapped(text, parent, encoded, Vec::new())
    }

    /// Retain a certain scalar mapping; invalid, incomplete or reordered mappings fail closed.
    /// Empty mappings explicitly mean that original-byte selection is unavailable.
    pub fn add_decoded_mapped(
        &mut self,
        text: String,
        parent: SourceId,
        encoded: Range<usize>,
        correspondence: Vec<Correspondence>,
    ) -> Option<SourceId> {
        let parent_text = &self.sources.get(parent.0)?.text;
        parent_text.get(encoded.clone())?;
        if !correspondence.is_empty() {
            let mut decoded_end = 0;
            let mut original_end = encoded.start;
            for mapping in &correspondence {
                if mapping.decoded.start != decoded_end
                    || text.get(mapping.decoded.clone())?.chars().count() != 1
                    || mapping.original.start < original_end
                    || mapping.original.end > encoded.end
                    || mapping.original.is_empty()
                {
                    return None;
                }
                parent_text.get(mapping.original.clone())?;
                decoded_end = mapping.decoded.end;
                original_end = mapping.original.end;
            }
            if decoded_end != text.len() {
                return None;
            }
        }
        let id = SourceId(self.sources.len());
        self.sources.push(Source {
            text,
            origin: SourceOrigin::Decoded { parent, encoded },
            correspondence,
        });
        Some(id)
    }

    /// Resolve a decoded range to exact original source ranges without inventing correspondence.
    /// Partial scalars, missing mappings or invalid sources return `None`. Disjoint encoded
    /// spans remain disjoint: quote delimiters and removed continuations are never guessed.
    #[allow(
        clippy::must_use_candidate,
        reason = "Option already carries must-use; repository policy forbids duplicating it"
    )]
    pub fn original_ranges(&self, id: SourceId, range: Range<usize>) -> Option<Vec<Range<usize>>> {
        let source = self.sources.get(id.0)?;
        source.text.get(range.clone())?;
        if source.origin == SourceOrigin::Original {
            return Some(vec![range]);
        }
        let SourceOrigin::Decoded { parent, .. } = source.origin else {
            return None;
        };
        if parent.0 >= id.0 {
            return None;
        }
        let mut offset = range.start;
        let mut ranges = Vec::new();
        let first = source
            .correspondence
            .partition_point(|mapping| mapping.decoded.start < range.start);
        for mapping in source.correspondence[first..]
            .iter()
            .take_while(|mapping| mapping.decoded.start < range.end)
        {
            if mapping.decoded.start >= range.start && mapping.decoded.end <= range.end {
                if mapping.decoded.start != offset {
                    return None;
                }
                ranges.extend(self.original_ranges(parent, mapping.original.clone())?);
                offset = mapping.decoded.end;
            }
        }
        (offset == range.end).then_some(ranges)
    }

    /// Append exact bytes from a validated source range; false leaves the document unchanged.
    pub fn push_source(&mut self, id: SourceId, range: Range<usize>, role: Role) -> bool {
        let Some(text) = self
            .sources
            .get(id.0)
            .and_then(|source| source.text.get(range.clone()))
        else {
            return false;
        };
        if !text.is_empty() {
            self.fragments.push(Fragment {
                text: text.to_owned(),
                role,
                provenance: Provenance::Source { id, range },
            });
        }
        true
    }

    /// Append presentation whitespace only; false rejects non-whitespace without mutation.
    pub fn push_generated(&mut self, text: &str) -> bool {
        if !text
            .bytes()
            .all(|byte| matches!(byte, b' ' | b'\t' | b'\r' | b'\n'))
        {
            return false;
        }
        if !text.is_empty() {
            self.fragments.push(Fragment {
                text: text.to_owned(),
                role: Role::Whitespace,
                provenance: Provenance::Generated,
            });
        }
        true
    }

    /// Split logical lines while retaining exact source ranges (including newline provenance).
    #[must_use]
    pub fn lines(&self) -> Vec<Line> {
        let mut lines = Vec::new();
        let mut line = Line::default();
        for fragment in &self.fragments {
            let mut offset = 0;
            for part in fragment.text.split_inclusive('\n') {
                let newline = part.ends_with('\n');
                let length = part.len() - usize::from(newline);
                if length != 0 {
                    line.fragments.push(Fragment {
                        text: part[..length].to_owned(),
                        role: fragment.role,
                        provenance: subrange(&fragment.provenance, offset..offset + length),
                    });
                }
                if newline {
                    line.newline = Some(subrange(
                        &fragment.provenance,
                        offset + length..offset + part.len(),
                    ));
                    lines.push(std::mem::take(&mut line));
                }
                offset += part.len();
            }
        }
        lines.push(line);
        lines
    }
}

const fn subrange(provenance: &Provenance, relative: Range<usize>) -> Provenance {
    match provenance {
        Provenance::Generated => Provenance::Generated,
        Provenance::Source { id, range } => Provenance::Source {
            id: *id,
            range: range.start + relative.start..range.start + relative.end,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_and_newline_provenance_survive_logical_splitting() {
        let mut document = Document::original("界\n😀\n");
        assert!(document.push_source(
            SourceId(0),
            0..document.execution_text().len(),
            Role::Literal
        ));
        let lines = document.lines();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[1].fragments[0].text, "😀");
        assert_eq!(
            lines[1].fragments[0].provenance,
            Provenance::Source {
                id: SourceId(0),
                range: 4..8
            }
        );
        assert_eq!(
            lines[1].newline,
            Some(Provenance::Source {
                id: SourceId(0),
                range: 8..9
            })
        );
    }

    #[test]
    fn scalar_correspondence_returns_original_encoded_bytes_not_decoded_guesses() {
        let mut document = Document::original("'a\\n界'");
        let id = document
            .add_decoded_mapped(
                "a\n界".to_owned(),
                SourceId(0),
                0..8,
                vec![
                    Correspondence {
                        decoded: 0..1,
                        original: 1..2,
                    },
                    Correspondence {
                        decoded: 1..2,
                        original: 2..4,
                    },
                    Correspondence {
                        decoded: 2..5,
                        original: 4..7,
                    },
                ],
            )
            .unwrap();
        assert_eq!(document.original_ranges(id, 1..5), Some(vec![2..4, 4..7]));
        assert_eq!(document.original_ranges(id, 3..5), None);
        let uncertain = document
            .add_decoded("a".to_owned(), SourceId(0), 0..8)
            .unwrap();
        assert_eq!(document.original_ranges(uncertain, 0..1), None);
        assert!(
            document
                .add_decoded_mapped(
                    "ab".to_owned(),
                    SourceId(0),
                    0..8,
                    vec![Correspondence {
                        decoded: 0..1,
                        original: 1..2
                    },]
                )
                .is_none()
        );
    }

    #[test]
    fn generated_text_cannot_become_source_authority() {
        let mut document = Document::original("echo 'a  b'");
        assert!(document.push_generated("\n  "));
        assert!(!document.push_generated("echo changed"));
        assert_eq!(document.execution_text(), "echo 'a  b'");
        assert_eq!(document.display_text(), "\n  ");
        assert!(
            document
                .add_decoded("a  b".to_owned(), SourceId(0), 5..11)
                .is_some()
        );
        assert!(
            document
                .add_decoded("bad".to_owned(), SourceId(99), 0..1)
                .is_none()
        );
    }
}
