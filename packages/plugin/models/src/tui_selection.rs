//! Producer-owned source geometry independent of native plugin loading.

/// One native visual selection cell.
///
/// Byte ranges must come from the producer's source model, never inferred by the host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginTuiSelectionCell {
    pub column: u16,
    pub width: u16,
    pub bytes: std::ops::Range<usize>,
}

/// Bounded source text and geometry for a visual row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginTuiSelectionRow {
    pub identity: String,
    pub byte_start: usize,
    pub text: String,
    pub cells: Vec<PluginTuiSelectionCell>,
    pub revision: u64,
}

impl PluginTuiSelectionRow {
    /// Whether bounded source metadata preserves UTF-8 and terminal cell boundaries.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        if self.identity.len() > 256 || self.text.len() > 256 * 1024 || self.cells.len() > 4096 {
            return false;
        }
        let Some(end) = self.byte_start.checked_add(self.text.len()) else {
            return false;
        };
        self.cells.iter().all(|cell| {
            cell.width > 0
                && cell.column.checked_add(cell.width).is_some()
                && cell.bytes.start >= self.byte_start
                && cell.bytes.end <= end
                && cell.bytes.start <= cell.bytes.end
                && self
                    .text
                    .is_char_boundary(cell.bytes.start - self.byte_start)
                && self.text.is_char_boundary(cell.bytes.end - self.byte_start)
        })
    }
}
