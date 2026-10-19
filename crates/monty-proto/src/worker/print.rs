//! Streaming of sandbox `print()` output as `Print` events.

use std::{
    borrow::Cow,
    mem,
    time::{Duration, Instant},
};

use monty_types::{ExcType, MontyException, PrintStream, PrintWriterCallback};

use super::{EventSink, event};
use crate::pb;

/// Streams sandbox `print()` output as `Print` events through an
/// [`EventSink`].
///
/// Debounced: a frame is written once the buffer reaches
/// [`Self::FLUSH_BYTES`] or its oldest byte has waited out `interval`, so the
/// number of events a turn produces follows elapsed time and output volume
/// rather than how many times the program called `print()` — which is what
/// makes a loop of tiny prints cost the parent (and its telemetry) roughly
/// what one large print costs. [`Self::drain`] empties the buffer before every
/// turn-ending event, so ordering against suspensions stays exact.
///
/// Output on both streams shares the buffer, held as one segment per run, so
/// alternating between them batches like any other output and still reaches
/// the parent in the order the sandbox produced it.
///
/// A zero `interval` disables the timer and restores line buffering, one event
/// per completed line.
pub(super) struct ProtoPrint<'a> {
    /// Buffered output, one segment per run on a single stream.
    segments: Vec<pb::PrintSegment>,
    /// Text bytes held across `segments`, so the size check stays O(1).
    buffered_bytes: usize,
    sink: &'a mut dyn EventSink,
    /// How long the oldest buffered byte may wait; zero means line buffering.
    interval: Duration,
    /// When the buffer stopped being empty; `None` while it is empty.
    buffered_since: Option<Instant>,
}

impl<'a> ProtoPrint<'a> {
    /// Flush threshold for output that never reaches the interval.
    const FLUSH_BYTES: usize = 8 * 1024;

    pub(super) fn new(sink: &'a mut dyn EventSink, interval: Duration) -> Self {
        Self {
            segments: Vec::new(),
            buffered_bytes: 0,
            sink,
            interval,
            buffered_since: None,
        }
    }

    /// Writes everything buffered (if anything) as one `Print` event.
    fn flush(&mut self) -> Result<(), MontyException> {
        if self.segments.is_empty() {
            return Ok(());
        }
        self.buffered_since = None;
        self.buffered_bytes = 0;
        let segments = mem::take(&mut self.segments);
        self.send(segments)
    }

    /// Emits one `Print` event per completed line, leaving any trailing
    /// partial line buffered. With the timer off the contract is one event per
    /// completed line, so a single write carrying embedded newlines has to be
    /// split rather than shipped whole; a line spanning a stream switch keeps
    /// its runs together in one event.
    fn flush_lines(&mut self) -> Result<(), MontyException> {
        while let Some((index, end)) = self.first_line_end() {
            let mut line: Vec<pb::PrintSegment> = self.segments.drain(..index).collect();
            let rest = self.segments[0].text.split_off(end);
            line.push(pb::PrintSegment {
                stream: self.segments[0].stream,
                text: mem::replace(&mut self.segments[0].text, rest),
            });
            if self.segments[0].text.is_empty() {
                self.segments.remove(0);
            }
            self.buffered_bytes -= line.iter().map(|segment| segment.text.len()).sum::<usize>();
            self.send(line)?;
        }
        if self.segments.is_empty() {
            self.buffered_since = None;
        }
        Ok(())
    }

    /// Where the first buffered line ends: the segment holding the `\n`, and
    /// the offset just past it. `None` while no line is complete.
    fn first_line_end(&self) -> Option<(usize, usize)> {
        self.segments
            .iter()
            .enumerate()
            .find_map(|(index, segment)| segment.text.find('\n').map(|at| (index, at + 1)))
    }

    /// Sends `segments` as one `Print` event.
    fn send(&mut self, segments: Vec<pb::PrintSegment>) -> Result<(), MontyException> {
        let event = event(pb::child_event::Kind::Print(pb::Print {
            segments: segments.into(),
        }));
        self.sink.send(&event).map_err(|err| {
            MontyException::new(
                ExcType::RuntimeError,
                Some(format!("failed to stream print output: {err}")),
            )
        })
    }

    /// Flushes whatever the buffer has earned: complete lines when the timer
    /// is off, then a frame if it has filled or its oldest byte has waited out
    /// `interval`.
    fn maybe_flush(&mut self) -> Result<(), MontyException> {
        // Lines leave first so the size threshold below cannot merge a
        // multi-line write back into one frame.
        if self.interval.is_zero() {
            self.flush_lines()?;
        }
        if self.buffered_bytes >= Self::FLUSH_BYTES || (!self.interval.is_zero() && self.interval_elapsed()) {
            self.flush()
        } else {
            Ok(())
        }
    }

    /// Whether buffered output has waited out `interval`. False when the
    /// buffer is empty, so an idle writer never reads the clock twice.
    fn interval_elapsed(&self) -> bool {
        self.buffered_since
            .is_some_and(|since| since.elapsed() >= self.interval)
    }

    /// Starts the interval clock when the buffer leaves the empty state.
    fn mark_buffered(&mut self) {
        if self.buffered_since.is_none() {
            self.buffered_since = Some(Instant::now());
        }
    }

    /// Flushes whatever is buffered; called before every turn-ending event.
    /// Errors are ignored — if the sink is broken the turn-ending write fails
    /// anyway.
    pub(super) fn drain(&mut self) {
        let _ = self.flush();
    }

    /// Buffers `text`, extending the trailing segment while it is on the same
    /// stream and starting a new one when the stream changes.
    fn append(&mut self, stream: PrintStream, text: &str) {
        self.mark_buffered();
        self.buffered_bytes += text.len();
        let stream = wire_stream(stream);
        match self.segments.last_mut() {
            Some(segment) if segment.stream == stream => segment.text.push_str(text),
            _ => self.segments.push(pb::PrintSegment {
                stream,
                text: text.to_owned(),
            }),
        }
    }

    fn write(&mut self, stream: PrintStream, output: &str) -> Result<(), MontyException> {
        // Append in pieces no larger than the flush threshold so a single huge
        // write cannot inflate the buffer (and the untracked copy it holds)
        // past `FLUSH_BYTES`: each filled chunk is flushed before the next is
        // appended.
        let mut rest = output;
        while !rest.is_empty() {
            let take = floor_char_boundary(rest, Self::FLUSH_BYTES - self.buffered_bytes);
            if take == 0 {
                // not even one char fits in the remaining room; flush to free
                // the whole threshold (far larger than any single char)
                self.flush()?;
                continue;
            }
            self.append(stream, &rest[..take]);
            rest = &rest[take..];
            self.maybe_flush()?;
        }
        Ok(())
    }

    fn push(&mut self, stream: PrintStream, end: char) -> Result<(), MontyException> {
        self.append(stream, end.encode_utf8(&mut [0; 4]));
        self.maybe_flush()
    }
}

/// Maps the interpreter's stream tag onto its wire enum value.
fn wire_stream(stream: PrintStream) -> i32 {
    match stream {
        PrintStream::Stdout => i32::from(pb::PrintStream::Stdout),
        PrintStream::Stderr => i32::from(pb::PrintStream::Stderr),
    }
}

impl PrintWriterCallback for ProtoPrint<'_> {
    fn stdout_write(&mut self, output: Cow<'_, str>) -> Result<(), MontyException> {
        self.write(PrintStream::Stdout, &output)
    }

    fn stdout_push(&mut self, end: char) -> Result<(), MontyException> {
        self.push(PrintStream::Stdout, end)
    }

    fn stderr_write(&mut self, output: Cow<'_, str>) -> Result<(), MontyException> {
        self.write(PrintStream::Stderr, &output)
    }

    fn stderr_push(&mut self, end: char) -> Result<(), MontyException> {
        self.push(PrintStream::Stderr, end)
    }

    /// Releases output the program stopped writing to: without this a script
    /// that prints and then computes in silence would hold that line until its
    /// next print or the end of the turn. A zero `interval` has no timer to
    /// expire, so line buffering is left to decide flushes on its own.
    fn poll_flush(&mut self) -> Result<(), MontyException> {
        if !self.interval.is_zero() && self.interval_elapsed() {
            self.flush()
        } else {
            Ok(())
        }
    }
}

/// Largest index `<= max` (capped at `s.len()`) that is a char boundary of
/// `s`, so `s[..idx]` is always valid UTF-8. A stable stand-in for the
/// unstable `str::floor_char_boundary`.
fn floor_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() {
        s.len()
    } else {
        let mut idx = max;
        // index 0 is always a boundary, so this terminates
        while !s.is_char_boundary(idx) {
            idx -= 1;
        }
        idx
    }
}
