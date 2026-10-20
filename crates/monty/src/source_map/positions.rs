//! Temporary coordinate index for source-free runtime compilation diagnostics.

use monty_types::CodeLoc;

use super::ResolvedLocation;
use crate::parse::CodeRange;

/// Resolves every compiled location without retaining source or repeatedly scanning long lines.
/// Non-ASCII characters contribute cumulative excess UTF-8 bytes, keeping ASCII indexing small.
pub(crate) struct SourcePositions {
    line_starts: Vec<u32>,
    unicode_excess: Vec<(u32, u32)>,
    source_len: u32,
}

impl SourcePositions {
    /// Indexes line breaks and Unicode column corrections once for all bodies in a compilation.
    pub(crate) fn new(source: &str) -> Self {
        let source_len = u32::try_from(source.len()).expect("parser limits source to 4 GiB");
        let mut line_starts = vec![0];
        let mut unicode_excess = Vec::new();
        let mut excess = 0;
        for (offset, c) in source.char_indices() {
            let end = u32::try_from(offset + c.len_utf8()).expect("offset fits source length");
            if c == '\n' {
                line_starts.push(end);
            } else if !c.is_ascii() {
                excess += u32::try_from(c.len_utf8() - 1).expect("UTF-8 character width fits u32");
                unicode_excess.push((end, excess));
            }
        }
        Self {
            line_starts,
            unicode_excess,
            source_len,
        }
    }

    /// Original line/column coordinates, without a source preview for eval/exec.
    pub(crate) fn resolve(&self, range: CodeRange) -> ResolvedLocation {
        ResolvedLocation {
            start: self.resolve_byte(range.start_byte),
            end: self.resolve_byte(range.end_byte),
            preview_line: None,
        }
    }

    /// Converts a compiler-produced UTF-8 boundary to a Unicode-scalar column.
    fn resolve_byte(&self, byte: u32) -> CodeLoc {
        let byte = byte.min(self.source_len);
        let line = self
            .line_starts
            .partition_point(|&start| start <= byte)
            .saturating_sub(1);
        let start = self.line_starts[line];
        let col = byte - start - (self.excess_before(byte) - self.excess_before(start));
        CodeLoc::new(u32::try_from(line).expect("line number fits source length"), col)
    }

    /// Number of UTF-8 continuation bytes preceding this boundary.
    fn excess_before(&self, byte: u32) -> u32 {
        let after = self.unicode_excess.partition_point(|&(end, _)| end <= byte);
        after.checked_sub(1).map_or(0, |index| self.unicode_excess[index].1)
    }
}
