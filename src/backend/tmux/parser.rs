//! Streaming parser for tmux control mode notifications.

use super::layout::{parse_layout, LayoutAst};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TmuxNotification {
    Output {
        pane_id: u64,
        data: Vec<u8>,
    },
    WindowAdd {
        window_id: u64,
    },
    WindowClose {
        window_id: u64,
    },
    PaneDied {
        pane_id: u64,
    },
    SessionChanged {
        raw: String,
    },
    LayoutChange {
        window_id: u64,
        layout: LayoutAst,
        raw: String,
    },
    NotificationParseError {
        kind: NotificationKind,
        raw: String,
        error: String,
    },
    Exit,
    CommandResponse(CommandResponse),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationKind {
    WindowAdd,
    WindowClose,
    LayoutChange,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResponse {
    pub begin: BlockMarker,
    pub status: CommandResponseStatus,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandResponseStatus {
    End(BlockMarker),
    Error(BlockMarker),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockMarker {
    pub raw: String,
    pub timestamp: Option<u64>,
    pub command_number: Option<u64>,
    pub flags: Option<u64>,
}

#[derive(Debug, Default)]
pub struct Parser {
    pending: Vec<u8>,
    block: Option<ActiveBlock>,
}

#[derive(Debug)]
struct ActiveBlock {
    begin: BlockMarker,
    lines: Vec<String>,
    nested_depth: usize,
}

impl Parser {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<TmuxNotification> {
        self.pending.extend_from_slice(chunk);

        let mut notifications = Vec::new();
        while let Some(line_end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let mut line = self.pending.drain(..=line_end).collect::<Vec<_>>();
            if line.ends_with(b"\n") {
                line.pop();
            }
            if line.ends_with(b"\r") {
                line.pop();
            }

            if let Some(notification) = self.parse_line(&line) {
                notifications.push(notification);
            }
        }

        notifications
    }

    fn parse_line(&mut self, line: &[u8]) -> Option<TmuxNotification> {
        if self.block.is_some() {
            return self.parse_block_line(line);
        }

        if let Some(block) = parse_begin(line) {
            self.block = Some(ActiveBlock {
                begin: block,
                lines: Vec::new(),
                nested_depth: 0,
            });
            return None;
        }

        if line.starts_with(b"%output") {
            return parse_output(line);
        }

        let text = String::from_utf8_lossy(line);
        let text = text.as_ref();

        if text.starts_with("%window-add") {
            return Some(parse_window_notification(
                raw_args(text, "%window-add"),
                NotificationKind::WindowAdd,
            ));
        }

        if text.starts_with("%unlinked-window-add") {
            return Some(parse_window_notification(
                raw_args(text, "%unlinked-window-add"),
                NotificationKind::WindowAdd,
            ));
        }

        if text.starts_with("%window-close") {
            return Some(parse_window_notification(
                raw_args(text, "%window-close"),
                NotificationKind::WindowClose,
            ));
        }

        if text.starts_with("%pane-died") {
            return raw_args(text, "%pane-died")
                .split_whitespace()
                .next()
                .and_then(|token| parse_id(token, '%'))
                .map(|pane_id| TmuxNotification::PaneDied { pane_id });
        }

        if text.starts_with("%message") {
            let raw = raw_args(text, "%message");
            if let Some(pane_id) = raw
                .strip_prefix("weave-pane-exited ")
                .and_then(|token| parse_id(token.trim(), '%'))
            {
                return Some(TmuxNotification::PaneDied { pane_id });
            }
        }

        if text.starts_with("%session-changed") {
            return Some(TmuxNotification::SessionChanged {
                raw: raw_args(text, "%session-changed").to_owned(),
            });
        }

        if text.starts_with("%layout-change") {
            return Some(parse_layout_change(raw_args(text, "%layout-change")));
        }

        if text.trim() == "%exit" {
            return Some(TmuxNotification::Exit);
        }

        None
    }

    fn parse_block_line(&mut self, line: &[u8]) -> Option<TmuxNotification> {
        let text = String::from_utf8_lossy(line).into_owned();

        if let Some(marker) = parse_begin(line) {
            if let Some(block) = &mut self.block {
                block.nested_depth = block.nested_depth.saturating_add(1);
                block.lines.push(format!("%begin {}", marker.raw));
            }
            return None;
        }

        let status = if text.starts_with("%end") {
            Some(CommandResponseStatus::End(parse_marker(raw_args(
                &text, "%end",
            ))))
        } else if text.starts_with("%error") {
            Some(CommandResponseStatus::Error(parse_marker(raw_args(
                &text, "%error",
            ))))
        } else {
            None
        };

        if let Some(status) = status {
            self.handle_block_status(text, status)
        } else {
            if let Some(block) = &mut self.block {
                block.lines.push(text);
            }
            None
        }
    }

    fn handle_block_status(
        &mut self,
        line: String,
        status: CommandResponseStatus,
    ) -> Option<TmuxNotification> {
        let block = self.block.as_mut()?;

        if block.nested_depth > 0 {
            block.nested_depth -= 1;
            block.lines.push(line);
            return None;
        }

        let block = self.block.take()?;
        Some(TmuxNotification::CommandResponse(CommandResponse {
            begin: block.begin,
            status,
            lines: block.lines,
        }))
    }
}

fn parse_begin(line: &[u8]) -> Option<BlockMarker> {
    let text = String::from_utf8_lossy(line);
    if !text.starts_with("%begin") {
        return None;
    }

    Some(parse_marker(raw_args(text.as_ref(), "%begin")))
}

fn parse_marker(raw: &str) -> BlockMarker {
    let mut fields = raw.split_whitespace();

    BlockMarker {
        raw: raw.to_owned(),
        timestamp: fields.next().and_then(|field| field.parse().ok()),
        command_number: fields.next().and_then(|field| field.parse().ok()),
        flags: fields.next().and_then(|field| field.parse().ok()),
    }
}

fn parse_output(line: &[u8]) -> Option<TmuxNotification> {
    let mut rest = line.strip_prefix(b"%output")?;
    rest = trim_ascii_start(rest);

    let pane_end = rest
        .iter()
        .position(u8::is_ascii_whitespace)
        .unwrap_or(rest.len());
    let pane_token = std::str::from_utf8(&rest[..pane_end]).ok()?;
    let pane_id = parse_id(pane_token, '%')?;
    let payload = if pane_end < rest.len() {
        &rest[pane_end + 1..]
    } else {
        &[]
    };

    Some(TmuxNotification::Output {
        pane_id,
        data: unescape_octal(payload),
    })
}

fn unescape_octal(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut index = 0;

    while index < input.len() {
        if input[index] == b'\\'
            && index + 3 < input.len()
            && input[index + 1..=index + 3]
                .iter()
                .all(|byte| byte.is_ascii_digit() && *byte < b'8')
        {
            let value = (input[index + 1] - b'0') * 64
                + (input[index + 2] - b'0') * 8
                + (input[index + 3] - b'0');
            output.push(value);
            index += 4;
        } else {
            output.push(input[index]);
            index += 1;
        }
    }

    output
}

fn trim_ascii_start(input: &[u8]) -> &[u8] {
    let first_non_space = input
        .iter()
        .position(|byte| !byte.is_ascii_whitespace())
        .unwrap_or(input.len());
    &input[first_non_space..]
}

fn raw_args<'a>(line: &'a str, command: &str) -> &'a str {
    line.strip_prefix(command).map_or("", str::trim_start)
}

fn parse_id(token: &str, prefix: char) -> Option<u64> {
    token.strip_prefix(prefix)?.parse().ok()
}

fn parse_window_notification(raw: &str, kind: NotificationKind) -> TmuxNotification {
    match parse_window_id(raw) {
        Ok(window_id) => match kind {
            NotificationKind::WindowAdd => TmuxNotification::WindowAdd { window_id },
            NotificationKind::WindowClose => TmuxNotification::WindowClose { window_id },
            NotificationKind::LayoutChange => unreachable!("layout-change is parsed separately"),
        },
        Err(error) => TmuxNotification::NotificationParseError {
            kind,
            raw: raw.to_owned(),
            error,
        },
    }
}

fn parse_layout_change(raw: &str) -> TmuxNotification {
    match parse_layout_change_fields(raw) {
        Ok((window_id, layout)) => TmuxNotification::LayoutChange {
            window_id,
            layout,
            raw: raw.to_owned(),
        },
        Err(error) => TmuxNotification::NotificationParseError {
            kind: NotificationKind::LayoutChange,
            raw: raw.to_owned(),
            error,
        },
    }
}

fn parse_layout_change_fields(raw: &str) -> Result<(u64, LayoutAst), String> {
    let mut fields = raw.split_whitespace();
    let window_token = fields
        .next()
        .ok_or_else(|| "missing window id".to_owned())?;
    let window_id = parse_window_id_token(window_token)?;
    let layout_token = fields
        .next()
        .ok_or_else(|| "missing visible layout".to_owned())?;
    let layout = parse_layout(layout_token).map_err(|error| error.to_string())?;

    Ok((window_id, layout))
}

fn parse_window_id(raw: &str) -> Result<u64, String> {
    let token = raw
        .split_whitespace()
        .next()
        .ok_or_else(|| "missing window id".to_owned())?;
    parse_window_id_token(token)
}

fn parse_window_id_token(token: &str) -> Result<u64, String> {
    let id = token
        .strip_prefix('@')
        .ok_or_else(|| format!("missing @ prefix in window id `{token}`"))?;
    id.parse::<u64>()
        .map_err(|error| format!("invalid window id `{token}`: {error}"))
}

#[cfg(test)]
mod tests {
    use crate::layout::geometry::Rect;

    use super::{
        BlockMarker, CommandResponse, CommandResponseStatus, LayoutAst, NotificationKind, Parser,
        TmuxNotification,
    };

    #[test]
    fn parses_output_and_unescapes_octal_payload() {
        let mut parser = Parser::new();

        assert_eq!(
            parser.feed(
                br"%output %42 hello\040world\012\134done
"
            ),
            vec![TmuxNotification::Output {
                pane_id: 42,
                data: b"hello world\n\\done".to_vec(),
            }]
        );
    }

    #[test]
    fn leaves_literal_output_text_untouched() {
        let mut parser = Parser::new();

        assert_eq!(
            parser.feed(b"%output %3 plain %begin text \\not-octal\n"),
            vec![TmuxNotification::Output {
                pane_id: 3,
                data: b"plain %begin text \\not-octal".to_vec(),
            }]
        );
    }

    #[test]
    fn parses_window_notifications() {
        let mut parser = Parser::new();

        assert_eq!(
            parser.feed(b"%window-add @7\n%window-close @8\n"),
            vec![
                TmuxNotification::WindowAdd { window_id: 7 },
                TmuxNotification::WindowClose { window_id: 8 },
            ]
        );
    }

    #[test]
    fn parses_pane_session_layout_and_exit_notifications() {
        let mut parser = Parser::new();
        let layout = render_layout(&leaf(2, rect(0, 0, 80, 24)));
        let input = format!(
            "%pane-died %9
%message weave-pane-exited %10
%session-changed $1 0
%layout-change @2 {layout} {layout} *
%exit
"
        );

        assert_eq!(
            parser.feed(input.as_bytes()),
            vec![
                TmuxNotification::PaneDied { pane_id: 9 },
                TmuxNotification::PaneDied { pane_id: 10 },
                TmuxNotification::SessionChanged {
                    raw: "$1 0".to_owned(),
                },
                TmuxNotification::LayoutChange {
                    window_id: 2,
                    layout: leaf(2, rect(0, 0, 80, 24)),
                    raw: format!("@2 {layout} {layout} *"),
                },
                TmuxNotification::Exit,
            ]
        );
    }

    #[test]
    fn decodes_window_add() {
        let mut parser = Parser::new();

        assert_eq!(
            parser.feed(b"%window-add @42\n"),
            vec![TmuxNotification::WindowAdd { window_id: 42 }]
        );
    }

    #[test]
    fn decodes_unlinked_window_add_as_window_add() {
        let mut parser = Parser::new();

        assert_eq!(
            parser.feed(b"%unlinked-window-add @5\n"),
            vec![TmuxNotification::WindowAdd { window_id: 5 }]
        );
    }

    #[test]
    fn decodes_window_close() {
        let mut parser = Parser::new();

        assert_eq!(
            parser.feed(b"%window-close @99\n"),
            vec![TmuxNotification::WindowClose { window_id: 99 }]
        );
    }

    #[test]
    fn reports_window_add_without_id_as_parse_error() {
        let mut parser = Parser::new();

        assert_eq!(
            parser.feed(b"%window-add\n"),
            vec![TmuxNotification::NotificationParseError {
                kind: NotificationKind::WindowAdd,
                raw: String::new(),
                error: "missing window id".to_owned(),
            }]
        );
    }

    #[test]
    fn reports_window_add_with_non_numeric_id_as_parse_error() {
        let mut parser = Parser::new();

        let notifications = parser.feed(b"%window-add @notanumber\n");

        assert_eq!(notifications.len(), 1);
        assert!(matches!(
            &notifications[0],
            TmuxNotification::NotificationParseError {
                kind: NotificationKind::WindowAdd,
                raw,
                ..
            } if raw == "@notanumber"
        ));
    }

    #[test]
    fn decodes_layout_change_visible_layout() {
        let mut parser = Parser::new();
        let ast = LayoutAst::Horizontal {
            rect: rect(0, 0, 80, 24),
            children: vec![leaf(0, rect(0, 0, 40, 24)), leaf(1, rect(40, 0, 40, 24))],
        };
        let visible = render_layout(&ast);
        let window = render_layout(&leaf(9, rect(0, 0, 80, 24)));
        let input = format!("%layout-change @0 {visible} {window} *\n");

        assert_eq!(
            parser.feed(input.as_bytes()),
            vec![TmuxNotification::LayoutChange {
                window_id: 0,
                layout: ast,
                raw: format!("@0 {visible} {window} *"),
            }]
        );
    }

    #[test]
    fn reports_layout_change_with_malformed_layout_as_parse_error() {
        let mut parser = Parser::new();

        let notifications = parser.feed(b"%layout-change @0 malformed-layout same-layout *\n");

        assert_eq!(notifications.len(), 1);
        assert!(matches!(
            &notifications[0],
            TmuxNotification::NotificationParseError {
                kind: NotificationKind::LayoutChange,
                raw,
                ..
            } if raw == "@0 malformed-layout same-layout *"
        ));
    }

    #[test]
    fn reports_layout_change_with_missing_window_id_as_parse_error() {
        let mut parser = Parser::new();

        assert_eq!(
            parser.feed(b"%layout-change\n"),
            vec![TmuxNotification::NotificationParseError {
                kind: NotificationKind::LayoutChange,
                raw: String::new(),
                error: "missing window id".to_owned(),
            }]
        );
    }

    #[test]
    fn emits_command_response_on_end() {
        let mut parser = Parser::new();

        assert_eq!(
            parser.feed(
                b"%begin 100 23 1
first line
%output %1 response text
%end 100 23 1
",
            ),
            vec![TmuxNotification::CommandResponse(CommandResponse {
                begin: marker("100 23 1"),
                status: CommandResponseStatus::End(marker("100 23 1")),
                lines: vec![
                    "first line".to_owned(),
                    "%output %1 response text".to_owned(),
                ],
            })]
        );
    }

    #[test]
    fn emits_command_response_on_error_inside_block() {
        let mut parser = Parser::new();

        assert_eq!(
            parser.feed(
                b"%begin 10 5 0
bad command
%error 10 5 0
",
            ),
            vec![TmuxNotification::CommandResponse(CommandResponse {
                begin: marker("10 5 0"),
                status: CommandResponseStatus::Error(marker("10 5 0")),
                lines: vec!["bad command".to_owned()],
            })]
        );
    }

    #[test]
    fn tracks_nested_blocks_before_closing_outer_response() {
        let mut parser = Parser::new();

        assert_eq!(
            parser.feed(
                b"%begin 1 1 0
outer before
%begin 2 2 0
inner text
%error 2 2 0
outer after
%end 1 1 0
",
            ),
            vec![TmuxNotification::CommandResponse(CommandResponse {
                begin: marker("1 1 0"),
                status: CommandResponseStatus::End(marker("1 1 0")),
                lines: vec![
                    "outer before".to_owned(),
                    "%begin 2 2 0".to_owned(),
                    "inner text".to_owned(),
                    "%error 2 2 0".to_owned(),
                    "outer after".to_owned(),
                ],
            })]
        );
    }

    #[test]
    fn ignores_malformed_and_garbage_input() {
        let mut parser = Parser::new();

        assert!(parser
            .feed(b"plain garbage\n%pane-died nope\n%output nope data\n\xFF\xFE\n")
            .is_empty());
    }

    #[test]
    fn buffers_split_mid_line_across_feeds() {
        let mut parser = Parser::new();

        assert!(parser.feed(b"%output %5 hel").is_empty());
        assert_eq!(
            parser.feed(b"lo\\012there\n"),
            vec![TmuxNotification::Output {
                pane_id: 5,
                data: b"hello\nthere".to_vec(),
            }]
        );
    }

    fn marker(raw: &str) -> BlockMarker {
        let mut fields = raw.split_whitespace();
        BlockMarker {
            raw: raw.to_owned(),
            timestamp: fields.next().and_then(|field| field.parse().ok()),
            command_number: fields.next().and_then(|field| field.parse().ok()),
            flags: fields.next().and_then(|field| field.parse().ok()),
        }
    }

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect { x, y, w, h }
    }

    fn leaf(pane_id: u64, rect: Rect) -> LayoutAst {
        LayoutAst::Leaf { pane_id, rect }
    }

    fn render_layout(ast: &LayoutAst) -> String {
        let mut body = String::new();
        render_node(ast, &mut body);
        format!("{:04x},{body}", layout_checksum(body.as_bytes()))
    }

    fn render_node(ast: &LayoutAst, out: &mut String) {
        match ast {
            LayoutAst::Leaf { pane_id, rect } => {
                push_rect(*rect, out);
                out.push(',');
                out.push_str(&pane_id.to_string());
            }
            LayoutAst::Horizontal { rect, children } => {
                push_rect(*rect, out);
                out.push('{');
                render_children(children, out);
                out.push('}');
            }
            LayoutAst::Vertical { rect, children } => {
                push_rect(*rect, out);
                out.push('[');
                render_children(children, out);
                out.push(']');
            }
        }
    }

    fn render_children(children: &[LayoutAst], out: &mut String) {
        for (idx, child) in children.iter().enumerate() {
            if idx > 0 {
                out.push(',');
            }
            render_node(child, out);
        }
    }

    fn push_rect(rect: Rect, out: &mut String) {
        out.push_str(&rect.w.to_string());
        out.push('x');
        out.push_str(&rect.h.to_string());
        out.push(',');
        out.push_str(&rect.x.to_string());
        out.push(',');
        out.push_str(&rect.y.to_string());
    }

    fn layout_checksum(bytes: &[u8]) -> u16 {
        let mut csum: u16 = 0;
        for &byte in bytes {
            csum = (csum >> 1) | ((csum & 1) << 15);
            csum = csum.wrapping_add(u16::from(byte));
        }
        csum
    }
}

#[cfg(test)]
mod proptests {
    use super::{Parser, TmuxNotification};
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 256,
            ..ProptestConfig::default()
        })]

        #[test]
        fn arbitrary_bytes_never_panic(chunks in byte_chunks()) {
            let mut parser = Parser::new();

            for chunk in chunks {
                let _notifications = parser.feed(&chunk);
            }
        }

        #[test]
        fn output_payload_roundtrip(pane_id in any::<u64>(), payload in output_payload()) {
            let mut line = format!("%output %{pane_id} ").into_bytes();
            line.extend(escape_payload(&payload));
            line.push(b'\n');

            let mut parser = Parser::new();
            prop_assert_eq!(
                parser.feed(&line),
                vec![TmuxNotification::Output {
                    pane_id,
                    data: payload,
                }]
            );
        }
    }

    fn byte_chunks() -> impl Strategy<Value = Vec<Vec<u8>>> {
        prop::collection::vec(any::<u8>(), 0..=1024).prop_flat_map(|input| {
            let len = input.len();
            (Just(input), 1usize..=3, 0usize..=len, 0usize..=len).prop_map(
                |(input, chunk_count, first, second)| match chunk_count {
                    1 => vec![input],
                    2 => vec![input[..first].to_vec(), input[first..].to_vec()],
                    3 => {
                        let start = first.min(second);
                        let end = first.max(second);
                        vec![
                            input[..start].to_vec(),
                            input[start..end].to_vec(),
                            input[end..].to_vec(),
                        ]
                    }
                    _ => unreachable!("chunk count strategy only produces 1..=3"),
                },
            )
        })
    }

    fn output_payload() -> impl Strategy<Value = Vec<u8>> {
        prop_oneof![
            prop::collection::vec(printable_ascii(), 0..=128),
            prop::collection::vec(control_byte(), 1..=64),
            prop::collection::vec(high_byte(), 1..=64),
            (
                prop::collection::vec(printable_ascii(), 1..=64),
                prop::collection::vec(control_byte(), 1..=32),
                prop::collection::vec(high_byte(), 1..=32),
            )
                .prop_map(|(mut ascii, mut controls, mut high_bytes)| {
                    ascii.append(&mut controls);
                    ascii.append(&mut high_bytes);
                    ascii
                }),
        ]
    }

    fn printable_ascii() -> impl Strategy<Value = u8> {
        prop_oneof![b' '..=b'[', b']'..=b'~']
    }

    fn control_byte() -> impl Strategy<Value = u8> {
        prop_oneof![0u8..=0x1f, Just(0x7f)]
    }

    fn high_byte() -> impl Strategy<Value = u8> {
        0x80u8..=u8::MAX
    }

    fn escape_payload(payload: &[u8]) -> Vec<u8> {
        let mut escaped = Vec::with_capacity(payload.len());

        for &byte in payload {
            if (b' '..=b'~').contains(&byte) && byte != b'\\' {
                escaped.push(byte);
            } else {
                escaped.push(b'\\');
                escaped.push(b'0' + byte / 64);
                escaped.push(b'0' + (byte / 8) % 8);
                escaped.push(b'0' + byte % 8);
            }
        }

        escaped
    }
}
