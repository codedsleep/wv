//! Terminal queries a pane's program expects its terminal to answer.
//!
//! A real terminal replies to a handful of probes, and programs block waiting
//! for those replies. The most important is Primary Device Attributes: shells
//! send a burst of feature probes and then a DA1 as a fence, holding their
//! prompt until the DA1 answer comes back, because every terminal answers it.
//! `vt100` models the screen and nothing else — it never replies — so a pane
//! that does not answer DA1 leaves its shell wedged before it ever draws a
//! prompt.
//!
//! Only queries weave can answer truthfully are intercepted. Probes for
//! features it does not implement (XTVERSION, XTGETTCAP, OSC background color)
//! are deliberately left unanswered: silence is how a terminal says
//! "unsupported", and the DA1 fence is what tells the program no more answers
//! are coming.
//!
//! The kitty keyboard protocol is the exception that is answered rather than
//! ignored. Programs use it to tell ESC apart from the start of an escape
//! sequence, and a program told "unsupported" falls back to timing guesses —
//! which is what makes vim-style modes in a pane feel unreliable. The requests
//! are per-pane state rather than one-shot answers, so they get their own
//! segment kind and are tracked by [`crate::term::pane::Pane`].

/// A query weave answers on a pane's behalf.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Query {
    /// `CSI c` / `CSI 0 c` — what kind of terminal are you?
    PrimaryDeviceAttributes,
    /// `CSI > c` — terminal firmware version.
    SecondaryDeviceAttributes,
    /// `CSI 5 n` — are you healthy?
    DeviceStatus,
    /// `CSI 6 n` — where is the cursor?
    CursorPosition,
}

/// One piece of a pane's output stream.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Segment {
    /// Bytes to feed to the emulator.
    Data(Vec<u8>),
    /// A query to answer. Its bytes are consumed, not forwarded: they carry no
    /// display meaning, and answering is the whole point.
    Query(Query),
    /// A kitty keyboard protocol request, changing or reporting the flags this
    /// pane's program wants its keys encoded with.
    Keyboard(KeyboardRequest),
}

/// A pane's kitty keyboard protocol request.
///
/// The protocol is a stack of flag sets: a program pushes what it wants on
/// entry and pops it on exit, so a nested program cannot leave the outer one
/// with settings it never asked for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyboardRequest {
    /// `CSI ? u` — report the flags currently in effect.
    Report,
    /// `CSI > flags u` — push a new flag set.
    Push(u8),
    /// `CSI < number u` — pop that many flag sets.
    Pop(u16),
    /// `CSI = flags ; mode u` — change the flags already on top of the stack.
    Set { flags: u8, mode: u8 },
}

/// Longest incomplete escape sequence held while waiting for the rest of it.
///
/// A string sequence that never terminates would otherwise swallow the stream,
/// so past this point the buffered bytes are released as ordinary data.
const MAX_PENDING: usize = 4096;

/// Splits a pane's output into data and the queries hiding in it.
///
/// PTY reads chop escape sequences at arbitrary points, so an unfinished
/// sequence at the end of a chunk is held back and completed by the next one.
#[derive(Default)]
pub struct QueryScanner {
    pending: Vec<u8>,
}

enum Parsed {
    /// A query, and how many bytes it occupies.
    Query(Query, usize),
    /// A kitty keyboard request, and how many bytes it occupies.
    Keyboard(KeyboardRequest, usize),
    /// A sequence the emulator does not implement, the bytes to feed it in
    /// place of the original, and how many bytes the original occupies.
    Rewritten(Vec<u8>, usize),
    /// Some other escape sequence, and how many bytes to skip over.
    Other(usize),
    /// The sequence runs past the end of the buffer.
    Incomplete,
}

impl QueryScanner {
    /// Split one chunk of pane output into segments, in stream order.
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Segment> {
        let mut buf = std::mem::take(&mut self.pending);
        buf.extend_from_slice(bytes);

        let mut segments = Vec::new();
        let mut data_start = 0;
        let mut index = 0;

        while index < buf.len() {
            if buf[index] != 0x1b {
                index += 1;
                continue;
            }

            match parse_escape(&buf[index..]) {
                Parsed::Query(query, length) => {
                    push_data(&mut segments, &buf[data_start..index]);
                    segments.push(Segment::Query(query));
                    index += length;
                    data_start = index;
                }
                Parsed::Keyboard(request, length) => {
                    push_data(&mut segments, &buf[data_start..index]);
                    segments.push(Segment::Keyboard(request));
                    index += length;
                    data_start = index;
                }
                Parsed::Rewritten(bytes, length) => {
                    push_data(&mut segments, &buf[data_start..index]);
                    segments.push(Segment::Data(bytes));
                    index += length;
                    data_start = index;
                }
                Parsed::Other(length) => index += length,
                Parsed::Incomplete => {
                    // Hold the tail for the next chunk, unless it has grown
                    // past the point where it is plausibly a real sequence.
                    if buf.len() - index <= MAX_PENDING {
                        break;
                    }
                    index += 1;
                }
            }
        }

        push_data(&mut segments, &buf[data_start..index]);
        self.pending = buf[index..].to_vec();

        segments
    }
}

fn push_data(segments: &mut Vec<Segment>, data: &[u8]) {
    if !data.is_empty() {
        segments.push(Segment::Data(data.to_vec()));
    }
}

fn parse_escape(buf: &[u8]) -> Parsed {
    match buf.get(1) {
        None => Parsed::Incomplete,
        Some(b'[') => parse_csi(buf),
        // OSC/DCS/SOS/PM/APC carry arbitrary text that could otherwise be
        // mistaken for a query, so they are skipped as a whole.
        Some(b']' | b'P' | b'X' | b'^' | b'_') => parse_string_sequence(buf),
        Some(_) => Parsed::Other(2),
    }
}

fn parse_csi(buf: &[u8]) -> Parsed {
    let mut index = 2;
    while let Some(&byte) = buf.get(index) {
        // Parameter bytes, then intermediate bytes, then one final byte.
        if (0x30..=0x3f).contains(&byte) || (0x20..=0x2f).contains(&byte) {
            index += 1;
            continue;
        }

        if (0x40..=0x7e).contains(&byte) {
            let params = &buf[2..index];
            let length = index + 1;

            return match (params, byte) {
                (b"" | b"0", b'c') => Parsed::Query(Query::PrimaryDeviceAttributes, length),
                (b">" | b">0", b'c') => Parsed::Query(Query::SecondaryDeviceAttributes, length),
                (b"5", b'n') => Parsed::Query(Query::DeviceStatus, length),
                (b"6", b'n') => Parsed::Query(Query::CursorPosition, length),
                (params, b'u') => match parse_keyboard_request(params) {
                    Some(request) => Parsed::Keyboard(request, length),
                    // A bare `CSI number u` is the protocol's *key* encoding,
                    // not a request. Nothing sends one upstream, so it is left
                    // alone rather than guessed at.
                    None => Parsed::Other(length),
                },
                // HVP is CUP under another name — same parameters, same
                // effect — but `vt100` implements only CUP and drops HVP on
                // the floor. A program that positions with HVP then paints its
                // whole interface from wherever the cursor happened to be:
                // btop uses HVP exclusively, and renders as a heap of
                // overlapping, wrapping boxes.
                (params, b'f') if is_numeric_params(params) => {
                    let mut rewritten = buf[..length].to_vec();
                    rewritten[length - 1] = b'H';
                    Parsed::Rewritten(rewritten, length)
                }
                _ => Parsed::Other(length),
            };
        }

        // Not a valid CSI byte at all: stop treating this as a sequence.
        return Parsed::Other(index);
    }

    Parsed::Incomplete
}

/// Read the parameter bytes of a `CSI ... u` as a keyboard protocol request.
///
/// Returns `None` for anything that is not one of the four request forms,
/// including the bare numeric form that encodes a key rather than asking for
/// anything.
fn parse_keyboard_request(params: &[u8]) -> Option<KeyboardRequest> {
    let (marker, rest) = params.split_first()?;

    match marker {
        b'?' if rest.is_empty() => Some(KeyboardRequest::Report),
        // A push with no number pushes an empty flag set, which is how a
        // program says "no enhancements, and remember what was here".
        b'>' => Some(KeyboardRequest::Push(parse_byte_param(rest, 0))),
        // Popping defaults to one level, not zero: `CSI < u` is the common
        // spelling of "undo my push".
        b'<' => Some(KeyboardRequest::Pop(parse_param(rest).unwrap_or(1))),
        b'=' => {
            let mut parts = rest.split(|byte| *byte == b';');
            let flags = parse_byte_param(parts.next().unwrap_or_default(), 0);
            let mode = parse_byte_param(parts.next().unwrap_or_default(), 1);
            Some(KeyboardRequest::Set { flags, mode })
        }
        _ => None,
    }
}

/// Parse a parameter that has to fit in a byte, as the flag and mode ones do.
///
/// Absent or out of range both fall back to `default`: every flag the protocol
/// defines fits in a byte, so a larger number is not a flag set weave could
/// honour even in part.
fn parse_byte_param(bytes: &[u8], default: u8) -> u8 {
    parse_param(bytes)
        .and_then(|value| u8::try_from(value).ok())
        .unwrap_or(default)
}

/// Parse one CSI parameter, saturating rather than wrapping on a long run of
/// digits. An empty parameter is absent, which the caller turns into a default.
fn parse_param(bytes: &[u8]) -> Option<u16> {
    if bytes.is_empty() || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }

    Some(
        bytes
            .iter()
            .fold(0u16, |value, byte| {
                value
                    .saturating_mul(10)
                    .saturating_add(u16::from(byte - b'0'))
            }),
    )
}

/// Whether a CSI's parameter bytes are a plain numeric list.
///
/// Anything else — a private marker like `?`, or an intermediate byte — means
/// the sequence is not the one it looks like, so it is left alone.
fn is_numeric_params(params: &[u8]) -> bool {
    params
        .iter()
        .all(|byte| byte.is_ascii_digit() || *byte == b';')
}

fn parse_string_sequence(buf: &[u8]) -> Parsed {
    let mut index = 2;
    while index < buf.len() {
        // BEL terminates an OSC; ST (ESC \) terminates any of them.
        if buf[index] == 0x07 {
            return Parsed::Other(index + 1);
        }
        if buf[index] == 0x1b {
            return match buf.get(index + 1) {
                Some(b'\\') => Parsed::Other(index + 2),
                Some(_) => Parsed::Other(index),
                None => Parsed::Incomplete,
            };
        }
        index += 1;
    }

    Parsed::Incomplete
}

/// The answer to send back up the pane's PTY.
///
/// `row` and `col` are zero-based, as `vt100` reports them; the wire format is
/// one-based.
pub fn reply(query: Query, row: u16, col: u16) -> Vec<u8> {
    match query {
        // "VT100 with Advanced Video Option", which is what tmux reports.
        Query::PrimaryDeviceAttributes => b"\x1b[?1;2c".to_vec(),
        Query::SecondaryDeviceAttributes => b"\x1b[>0;10;1c".to_vec(),
        // Terminal ready, no malfunction.
        Query::DeviceStatus => b"\x1b[0n".to_vec(),
        Query::CursorPosition => format!(
            "\x1b[{};{}R",
            row.saturating_add(1),
            col.saturating_add(1)
        )
        .into_bytes(),
    }
}

/// The answer to `CSI ? u`: the flags currently in effect for that pane.
pub fn keyboard_reply(flags: u8) -> Vec<u8> {
    format!("\x1b[?{flags}u").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::{reply, KeyboardRequest, Query, QueryScanner, Segment};

    fn data(bytes: &[u8]) -> Segment {
        Segment::Data(bytes.to_vec())
    }

    #[test]
    fn plain_output_passes_through_untouched() {
        let mut scanner = QueryScanner::default();

        assert_eq!(scanner.feed(b"hello world"), vec![data(b"hello world")]);
    }

    #[test]
    fn primary_device_attributes_is_intercepted_and_removed() {
        let mut scanner = QueryScanner::default();

        assert_eq!(
            scanner.feed(b"before\x1b[0cafter"),
            vec![
                data(b"before"),
                Segment::Query(Query::PrimaryDeviceAttributes),
                data(b"after"),
            ]
        );
    }

    #[test]
    fn bare_primary_device_attributes_is_intercepted() {
        let mut scanner = QueryScanner::default();

        assert_eq!(
            scanner.feed(b"\x1b[c"),
            vec![Segment::Query(Query::PrimaryDeviceAttributes)]
        );
    }

    #[test]
    fn secondary_attributes_status_and_cursor_position_are_intercepted() {
        let mut scanner = QueryScanner::default();

        assert_eq!(
            scanner.feed(b"\x1b[>c\x1b[5n\x1b[6n"),
            vec![
                Segment::Query(Query::SecondaryDeviceAttributes),
                Segment::Query(Query::DeviceStatus),
                Segment::Query(Query::CursorPosition),
            ]
        );
    }

    #[test]
    fn ordinary_escape_sequences_are_left_in_the_stream() {
        let mut scanner = QueryScanner::default();

        assert_eq!(
            scanner.feed(b"\x1b[31mred\x1b[m\x1b[2J"),
            vec![data(b"\x1b[31mred\x1b[m\x1b[2J")]
        );
    }

    /// The exact burst fish sends after its greeting: feature probes fenced by
    /// the DA1 it blocks on. The keyboard probe that opens it is answered too;
    /// the rest are not, and reach the emulator untouched.
    #[test]
    fn a_shell_probe_burst_answers_the_da1_fence_and_the_keyboard_probe() {
        let mut scanner = QueryScanner::default();
        let burst = b"\x1b[?u\x1b[>0q\x1b]11;?\x1b\\\x1b[?1049h\x1bP+q696e646e\x1b\\\x1b[?1049l\x1b[0c";

        let segments = scanner.feed(burst);

        let queries: Vec<_> = segments
            .iter()
            .filter_map(|segment| match segment {
                Segment::Query(query) => Some(*query),
                _ => None,
            })
            .collect();
        assert_eq!(queries, vec![Query::PrimaryDeviceAttributes]);

        let keyboard: Vec<_> = segments
            .iter()
            .filter_map(|segment| match segment {
                Segment::Keyboard(request) => Some(*request),
                _ => None,
            })
            .collect();
        assert_eq!(keyboard, vec![KeyboardRequest::Report]);

        // Everything else still reaches the emulator, in order: the burst
        // without the four leading bytes of the keyboard probe and the four
        // trailing ones of the DA1.
        let forwarded: Vec<u8> = segments
            .iter()
            .filter_map(|segment| match segment {
                Segment::Data(bytes) => Some(bytes.clone()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(forwarded, &burst[4..burst.len() - 4]);
    }

    #[test]
    fn keyboard_requests_are_parsed_in_all_four_forms() {
        let mut scanner = QueryScanner::default();

        assert_eq!(
            scanner.feed(b"\x1b[?u\x1b[>5u\x1b[<2u\x1b[=3;2u"),
            vec![
                Segment::Keyboard(KeyboardRequest::Report),
                Segment::Keyboard(KeyboardRequest::Push(5)),
                Segment::Keyboard(KeyboardRequest::Pop(2)),
                Segment::Keyboard(KeyboardRequest::Set { flags: 3, mode: 2 }),
            ]
        );
    }

    #[test]
    fn keyboard_requests_default_their_omitted_parameters() {
        let mut scanner = QueryScanner::default();

        assert_eq!(
            scanner.feed(b"\x1b[>u\x1b[<u\x1b[=4u"),
            vec![
                // A push with no flags is a push of none.
                Segment::Keyboard(KeyboardRequest::Push(0)),
                // A pop with no count pops one level, not zero.
                Segment::Keyboard(KeyboardRequest::Pop(1)),
                // A set with no mode replaces the flags outright.
                Segment::Keyboard(KeyboardRequest::Set { flags: 4, mode: 1 }),
            ]
        );
    }

    /// `CSI number u` is how the protocol encodes a *key*, not a request about
    /// it. Nothing sends one to a terminal, and treating it as a request would
    /// swallow a byte sequence that belongs to the emulator.
    #[test]
    fn a_bare_numeric_csi_u_is_not_a_keyboard_request() {
        let mut scanner = QueryScanner::default();

        assert_eq!(scanner.feed(b"\x1b[27u"), vec![data(b"\x1b[27u")]);
    }

    /// `vt100` implements CUP but not HVP, so HVP has to arrive as CUP or the
    /// move is silently dropped.
    #[test]
    fn hvp_is_rewritten_to_cup() {
        let mut scanner = QueryScanner::default();

        assert_eq!(
            scanner.feed(b"a\x1b[3;5fb"),
            vec![data(b"a"), data(b"\x1b[3;5H"), data(b"b")]
        );
    }

    #[test]
    fn hvp_without_parameters_is_still_rewritten() {
        let mut scanner = QueryScanner::default();

        assert_eq!(scanner.feed(b"\x1b[f"), vec![data(b"\x1b[H")]);
    }

    #[test]
    fn an_hvp_split_across_reads_is_still_rewritten() {
        let mut scanner = QueryScanner::default();

        assert_eq!(scanner.feed(b"out\x1b[12;"), vec![data(b"out")]);
        assert_eq!(scanner.feed(b"34f"), vec![data(b"\x1b[12;34H")]);
    }

    /// Only a plain numeric HVP is CUP by another name. A private-marker
    /// sequence that happens to end in `f` means something else entirely.
    #[test]
    fn a_private_sequence_ending_in_f_is_left_alone() {
        let mut scanner = QueryScanner::default();

        assert_eq!(scanner.feed(b"\x1b[?7f"), vec![data(b"\x1b[?7f")]);
    }

    #[test]
    fn a_query_split_across_reads_is_still_recognized() {
        let mut scanner = QueryScanner::default();

        assert_eq!(scanner.feed(b"out\x1b["), vec![data(b"out")]);
        assert_eq!(
            scanner.feed(b"0c"),
            vec![Segment::Query(Query::PrimaryDeviceAttributes)]
        );
    }

    #[test]
    fn a_string_sequence_payload_is_not_scanned_for_queries() {
        let mut scanner = QueryScanner::default();
        // A DCS XTGETTCAP probe: hex payload, terminated by ST, no reply owed.
        let dcs = b"\x1bP+q696e646e\x1b\\after";

        assert_eq!(scanner.feed(dcs), vec![data(dcs)]);
    }

    /// An ESC inside a string sequence aborts it and starts a new sequence, as
    /// it does in xterm. Keeping that behavior means an unterminated OSC cannot
    /// hide the DA1 fence a shell is blocked on.
    #[test]
    fn an_escape_inside_a_string_sequence_aborts_it() {
        let mut scanner = QueryScanner::default();

        assert_eq!(
            scanner.feed(b"\x1b]2;title\x1b[0c"),
            vec![
                data(b"\x1b]2;title"),
                Segment::Query(Query::PrimaryDeviceAttributes),
            ]
        );
    }

    #[test]
    fn an_unterminated_string_sequence_does_not_swallow_the_stream() {
        let mut scanner = QueryScanner::default();
        let flood = vec![b'x'; super::MAX_PENDING + 16];

        let mut input = b"\x1b]2;".to_vec();
        input.extend_from_slice(&flood);
        let segments = scanner.feed(&input);

        assert!(
            !segments.is_empty(),
            "buffered bytes must eventually be released"
        );
    }

    #[test]
    fn cursor_position_reply_is_one_based() {
        assert_eq!(reply(Query::CursorPosition, 0, 0), b"\x1b[1;1R".to_vec());
        assert_eq!(reply(Query::CursorPosition, 4, 9), b"\x1b[5;10R".to_vec());
    }

    #[test]
    fn device_attribute_replies_are_well_formed() {
        assert_eq!(
            reply(Query::PrimaryDeviceAttributes, 0, 0),
            b"\x1b[?1;2c".to_vec()
        );
        assert_eq!(
            reply(Query::SecondaryDeviceAttributes, 0, 0),
            b"\x1b[>0;10;1c".to_vec()
        );
        assert_eq!(reply(Query::DeviceStatus, 0, 0), b"\x1b[0n".to_vec());
    }
}
