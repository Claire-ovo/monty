//! Pre-parse bound on how deeply the parser and compiler may recurse.
//!
//! oxc parses and the compiler walks the tree on the Rust stack, so a source
//! nested a few thousand levels deep (`((((…`, `a => a => …`, `!!!!…`,
//! `1+1+1+…`, `else if … else if …`) aborts the worker before any limit the
//! interpreter enforces can apply. One pass over the characters estimates
//! the recursion every construct will cost and never undercounts it, so a
//! source past [`MAX_NESTING_DEPTH`] is rejected as a `SyntaxError` instead.
//! The estimate is deliberately rough: it may reject a convoluted but legal
//! source well before the real limit, never accept one that overflows.

use std::mem;

/// The deepest estimated nesting a feed may have.
#[cfg(not(debug_assertions))]
pub(crate) const MAX_NESTING_DEPTH: u32 = 500;
/// Debug frames are several times larger, so the bound is tighter.
#[cfg(debug_assertions)]
pub(crate) const MAX_NESTING_DEPTH: u32 = 150;

/// The message of the `SyntaxError` a too-deep source raises.
pub(crate) const TOO_DEEP: &str = "Source is too deeply nested";

/// The byte offset at which the estimated nesting first exceeds the limit,
/// or `None` when the source is fine.
pub(crate) fn nesting_bound_exceeded(source: &str) -> Option<usize> {
    // every counted construct costs at least one byte, so a short source cannot exceed the bound
    if source.len() <= MAX_NESTING_DEPTH as usize {
        return None;
    }
    Scanner::new(source).run()
}

/// What opened a bracket level.
#[derive(Clone, Copy, PartialEq, Eq)]
enum LevelKind {
    Top,
    Paren,
    Bracket,
    Brace,
    /// A `${` inside a template literal: its `}` resumes the template text.
    TemplateHole,
}

/// The frames charged at one bracket level.
struct Level {
    kind: LevelKind,
    /// Frames an expression or a brace-less statement holds: prefix and
    /// binary operators, member and call chains, arrows, conditionals and
    /// `else if` chains. Released by `;`, `,` or an automatic semicolon.
    frames: u32,
    /// Statement keywords (`if`, `function`, …) whose block has not opened;
    /// moved into the block's level when its `{` arrives.
    pending: u32,
    /// Frames the level inherited from its opening keyword, released with it.
    carried: u32,
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "independent lexer facts, not a state machine"
)]
struct Scanner<'s> {
    source: &'s [u8],
    pos: usize,
    levels: Vec<Level>,
    /// Open levels plus every charged frame.
    total: u32,
    /// Whether the previous token can end an operand (so `/` divides and `-` is binary).
    prev_operand: bool,
    /// Whether the previous token was a `}` closing a block.
    prev_block_end: bool,
    /// Whether a line break separates the previous token from the next.
    newline: bool,
    /// An `else` is waiting to see whether `if` or `{` follows.
    after_else: bool,
    /// A `case` test is open, so the next `:` ends it rather than labelling a statement.
    in_case: bool,
}

impl<'s> Scanner<'s> {
    fn new(source: &'s str) -> Self {
        Self {
            source: source.as_bytes(),
            pos: 0,
            levels: vec![Level {
                kind: LevelKind::Top,
                frames: 0,
                pending: 0,
                carried: 0,
            }],
            total: 1,
            prev_operand: false,
            prev_block_end: false,
            newline: false,
            after_else: false,
            in_case: false,
        }
    }

    fn run(mut self) -> Option<usize> {
        while self.pos < self.source.len() {
            let at = self.pos;
            if self.step().is_err() {
                return Some(at);
            }
        }
        None
    }

    /// Consumes one token (or run of whitespace / a comment), charging what it opens.
    fn step(&mut self) -> Result<(), ()> {
        let byte = self.source[self.pos];
        match byte {
            b'\n' | b'\r' => {
                self.newline = true;
                self.pos += 1;
                return Ok(());
            }
            b' ' | b'\t' | 0x0B | 0x0C => {
                self.pos += 1;
                return Ok(());
            }
            _ => {}
        }
        let token_start = self.pos;
        // an automatic semicolon: a new statement after a line break ends the previous one's frames
        if self.newline && (self.prev_operand || self.prev_block_end) && starts_statement(&self.source[self.pos..]) {
            self.release_expression();
        }
        self.newline = false;
        let was_after_else = self.after_else;
        self.after_else = false;
        let mut ends_operand = false;
        let mut block_end = false;
        match byte {
            b'/' if self.peek(1) == Some(b'/') => {
                while self.pos < self.source.len() && !matches!(self.source[self.pos], b'\n' | b'\r') {
                    self.pos += 1;
                }
                return Ok(());
            }
            b'/' if self.peek(1) == Some(b'*') => {
                self.pos += 2;
                while self.pos < self.source.len() && !(self.source[self.pos] == b'*' && self.peek(1) == Some(b'/')) {
                    self.pos += 1;
                }
                self.pos = (self.pos + 2).min(self.source.len());
                return Ok(());
            }
            b'/' if !self.prev_operand => {
                self.skip_regex();
                ends_operand = true;
            }
            b'\'' | b'"' => {
                self.skip_string(byte);
                ends_operand = true;
            }
            b'`' => {
                self.pos += 1;
                self.skip_template_text()?;
                // either the template ended (an operand) or a `${` opened a level
                ends_operand = self
                    .levels
                    .last()
                    .is_none_or(|level| level.kind != LevelKind::TemplateHole)
                    || self.source.get(self.pos.wrapping_sub(1)) == Some(&b'`');
            }
            b'(' | b'[' => {
                if self.prev_operand {
                    // a call or computed member: the chain recurses on the callee
                    self.charge(1)?;
                }
                let kind = if byte == b'(' {
                    LevelKind::Paren
                } else {
                    LevelKind::Bracket
                };
                self.open(kind)?;
                self.pos += 1;
            }
            b'{' => {
                if was_after_else {
                    self.level_mut().pending += 1;
                    self.total += 1;
                }
                self.open(LevelKind::Brace)?;
                self.pos += 1;
            }
            b')' | b']' => {
                self.close();
                ends_operand = true;
                self.pos += 1;
            }
            b'}' => {
                let kind = self.close();
                self.pos += 1;
                if kind == LevelKind::TemplateHole {
                    self.skip_template_text()?;
                    ends_operand = self
                        .levels
                        .last()
                        .is_none_or(|level| level.kind != LevelKind::TemplateHole);
                } else {
                    block_end = true;
                }
            }
            b';' => {
                self.release_expression();
                let pending = mem::take(&mut self.level_mut().pending);
                self.total -= pending;
                self.pos += 1;
            }
            b',' => {
                self.release_expression();
                self.pos += 1;
            }
            b'0'..=b'9' => {
                while self.pos < self.source.len() && is_word_byte(self.source[self.pos]) || self.peek(0) == Some(b'.')
                {
                    self.pos += 1;
                }
                ends_operand = true;
            }
            _ if is_word_start(byte) => {
                let start = self.pos;
                while self.pos < self.source.len() && is_word_byte(self.source[self.pos]) {
                    self.pos += 1;
                }
                let word = &self.source[start..self.pos];
                match word {
                    b"typeof" | b"void" | b"delete" | b"await" | b"yield" | b"new" => self.charge(1)?,
                    b"instanceof" | b"in" => self.charge(1)?,
                    b"if" | b"for" | b"while" | b"do" | b"try" | b"switch" | b"with" | b"function" | b"class"
                    | b"catch" | b"finally" | b"static" => {
                        // an `else if` chain recurses in the compiler; a plain keyword waits for its block
                        if was_after_else && word == b"if" {
                            self.charge(1)?;
                        } else {
                            self.level_mut().pending += 1;
                            self.total += 1;
                            if self.total > MAX_NESTING_DEPTH {
                                return Err(());
                            }
                        }
                    }
                    b"else" => self.after_else = true,
                    b"case" => self.in_case = true,
                    b"return" | b"throw" | b"default" | b"break" | b"continue" | b"var" | b"let" | b"const"
                    | b"export" | b"import" | b"extends" => {}
                    _ => ends_operand = true,
                }
            }
            b'=' if self.peek(1) == Some(b'>') => {
                self.charge(1)?;
                self.pos += 2;
            }
            b'?' if self.peek(1) == Some(b'.') => {
                self.charge(1)?;
                self.pos += 2;
            }
            b'?' if self.peek(1) == Some(b'?') => {
                self.charge(1)?;
                self.pos += 2;
            }
            b'?' | b'.' | b'=' | b'+' | b'-' | b'*' | b'%' | b'<' | b'>' | b'&' | b'|' | b'^' | b'!' | b'~' | b'/' => {
                // every operator starts a nested operand: prefix ones on the right,
                // binary ones through the chain they extend
                self.charge(1)?;
                self.pos += 1;
                while self.pos < self.source.len()
                    && matches!(
                        self.source[self.pos],
                        b'=' | b'+' | b'-' | b'*' | b'<' | b'>' | b'&' | b'|'
                    )
                {
                    self.pos += 1;
                }
            }
            b':' => {
                // a label (`l: statement`) nests the statement in the parser and the compiler;
                // the `:` ending a `case` test does not, and charging a ternary's or an
                // object key's only overcounts until the `,` or `;` releases it
                if !mem::take(&mut self.in_case) && self.prev_operand {
                    self.charge(1)?;
                }
                self.pos += 1;
            }
            b'#' | b'@' | b'\\' => self.pos += 1,
            _ => {
                // a non-ASCII byte: part of an identifier or a string the lexer already skipped
                self.pos += 1;
                ends_operand = true;
            }
        }
        if self.pos == token_start {
            self.pos += 1;
        }
        self.prev_operand = ends_operand;
        self.prev_block_end = block_end;
        Ok(())
    }

    fn peek(&self, offset: usize) -> Option<u8> {
        self.source.get(self.pos + offset).copied()
    }

    fn level_mut(&mut self) -> &mut Level {
        self.levels.last_mut().expect("the top level is never popped")
    }

    /// Charges `n` expression frames at the current level.
    fn charge(&mut self, n: u32) -> Result<(), ()> {
        self.level_mut().frames += n;
        self.total += n;
        if self.total > MAX_NESTING_DEPTH {
            Err(())
        } else {
            Ok(())
        }
    }

    /// Opens a bracket level, moving the keyword frames waiting for it inside.
    fn open(&mut self, kind: LevelKind) -> Result<(), ()> {
        let parent = self.level_mut();
        let carried = if kind == LevelKind::Brace {
            mem::take(&mut parent.pending)
        } else {
            0
        };
        self.levels.push(Level {
            kind,
            frames: 0,
            pending: 0,
            carried,
        });
        self.total += 1;
        if self.total > MAX_NESTING_DEPTH {
            Err(())
        } else {
            Ok(())
        }
    }

    /// Closes the current level, releasing everything it held.
    fn close(&mut self) -> LevelKind {
        if self.levels.len() == 1 {
            // an unbalanced closer: the parser will reject it; keep scanning
            return LevelKind::Top;
        }
        let level = self.levels.pop().expect("more than the top level");
        self.total -= 1 + level.frames + level.pending + level.carried;
        level.kind
    }

    /// The end of an expression: its operator and chain frames return.
    fn release_expression(&mut self) {
        let level = self.level_mut();
        let frames = level.frames;
        level.frames = 0;
        self.total -= frames;
    }

    fn skip_string(&mut self, quote: u8) {
        self.pos += 1;
        while self.pos < self.source.len() {
            match self.source[self.pos] {
                b'\\' => self.pos += 2,
                b'\n' => return,
                byte if byte == quote => {
                    self.pos += 1;
                    return;
                }
                _ => self.pos += 1,
            }
        }
    }

    /// Template text up to the closing backtick (consumed) or a `${`, which
    /// opens a level the main loop scans.
    fn skip_template_text(&mut self) -> Result<(), ()> {
        while self.pos < self.source.len() {
            match self.source[self.pos] {
                b'\\' => self.pos += 2,
                b'`' => {
                    self.pos += 1;
                    return Ok(());
                }
                b'$' if self.peek(1) == Some(b'{') => {
                    self.pos += 2;
                    return self.open(LevelKind::TemplateHole);
                }
                _ => self.pos += 1,
            }
        }
        Ok(())
    }

    fn skip_regex(&mut self) {
        self.pos += 1;
        let mut in_class = false;
        while self.pos < self.source.len() {
            match self.source[self.pos] {
                b'\\' => self.pos += 2,
                b'\n' => return,
                b'[' => {
                    in_class = true;
                    self.pos += 1;
                }
                b']' => {
                    in_class = false;
                    self.pos += 1;
                }
                b'/' if !in_class => {
                    self.pos += 1;
                    while self.pos < self.source.len() && is_word_byte(self.source[self.pos]) {
                        self.pos += 1;
                    }
                    return;
                }
                _ => self.pos += 1,
            }
        }
    }
}

/// Whether the text at a line start begins a new statement rather than
/// continuing the expression before the line break (`.x`, `+ y`, `(…)`).
fn starts_statement(rest: &[u8]) -> bool {
    match rest.first() {
        Some(byte) => {
            is_word_start(*byte)
                || byte.is_ascii_digit()
                || matches!(byte, b'\'' | b'"' | b'`' | b'!' | b'~' | b'{' | b'}' | b';')
        }
        None => false,
    }
}

fn is_word_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte == b'$' || byte >= 0x80
}

fn is_word_byte(byte: u8) -> bool {
    is_word_start(byte) || byte.is_ascii_digit()
}
