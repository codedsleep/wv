//! Parser for tmux layout strings.

use crate::layout::geometry::Rect;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LayoutAst {
    Leaf {
        pane_id: u64,
        rect: Rect,
    },
    Horizontal {
        rect: Rect,
        children: Vec<LayoutAst>,
    },
    Vertical {
        rect: Rect,
        children: Vec<LayoutAst>,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum LayoutParseError {
    #[error("layout string is empty")]
    Empty,
    #[error("missing or malformed checksum prefix")]
    BadChecksumPrefix,
    #[error("checksum mismatch: expected {expected:04x}, computed {computed:04x}")]
    ChecksumMismatch { expected: u16, computed: u16 },
    #[error("unexpected end of input while parsing {context}")]
    UnexpectedEnd { context: &'static str },
    #[error("expected `{expected}` at position {pos}, found `{found}`")]
    UnexpectedChar {
        expected: char,
        pos: usize,
        found: char,
    },
    #[error("invalid integer in {field}: {source}")]
    InvalidInt {
        field: &'static str,
        #[source]
        source: std::num::ParseIntError,
    },
    #[error("unbalanced group brackets")]
    UnbalancedGroup,
    #[error("trailing input after parse: `{0}`")]
    TrailingInput(String),
    #[error("empty group `{open}{close}`")]
    EmptyGroup { open: char, close: char },
}

pub fn parse_layout(input: &str) -> Result<LayoutAst, LayoutParseError> {
    if input.is_empty() {
        return Err(LayoutParseError::Empty);
    }

    let (checksum_prefix, body) = input
        .split_once(',')
        .ok_or(LayoutParseError::BadChecksumPrefix)?;
    if checksum_prefix.len() != 4
        || !checksum_prefix
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(LayoutParseError::BadChecksumPrefix);
    }

    let expected = u16::from_str_radix(checksum_prefix, 16)
        .map_err(|_| LayoutParseError::BadChecksumPrefix)?;
    let computed = layout_checksum(body.as_bytes());
    if expected != computed {
        return Err(LayoutParseError::ChecksumMismatch { expected, computed });
    }

    let mut parser = Parser::new(body);
    let ast = parser.parse_node()?;
    if !parser.is_done() {
        return Err(LayoutParseError::TrailingInput(
            parser.remaining().to_owned(),
        ));
    }

    Ok(ast)
}

fn layout_checksum(bytes: &[u8]) -> u16 {
    let mut csum: u16 = 0;
    for &byte in bytes {
        csum = (csum >> 1) | ((csum & 1) << 15);
        csum = csum.wrapping_add(u16::from(byte));
    }
    csum
}

struct Parser<'a> {
    input: &'a str,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(input: &'a str) -> Self {
        Self { input, pos: 0 }
    }

    fn parse_node(&mut self) -> Result<LayoutAst, LayoutParseError> {
        let w = self.parse_u16("width")?;
        self.expect_char('x')?;
        let h = self.parse_u16("height")?;
        self.expect_char(',')?;
        let x = self.parse_u16("x")?;
        self.expect_char(',')?;
        let y = self.parse_u16("y")?;
        let rect = Rect { x, y, w, h };

        match self.peek_char() {
            Some(',') => {
                self.advance_char();
                let pane_id = self.parse_u64("pane_id")?;
                Ok(LayoutAst::Leaf { pane_id, rect })
            }
            Some('{') => {
                let children = self.parse_group('{', '}')?;
                Ok(LayoutAst::Horizontal { rect, children })
            }
            Some('[') => {
                let children = self.parse_group('[', ']')?;
                Ok(LayoutAst::Vertical { rect, children })
            }
            Some(found) => Err(LayoutParseError::UnexpectedChar {
                expected: ',',
                pos: self.pos,
                found,
            }),
            None => Err(LayoutParseError::UnexpectedEnd {
                context: "node tail",
            }),
        }
    }

    fn parse_group(&mut self, open: char, close: char) -> Result<Vec<LayoutAst>, LayoutParseError> {
        self.expect_char(open)?;
        if self.peek_char() == Some(close) {
            self.advance_char();
            return Err(LayoutParseError::EmptyGroup { open, close });
        }

        let mut children = Vec::new();
        loop {
            children.push(self.parse_node()?);
            match self.peek_char() {
                Some(',') => {
                    self.advance_char();
                }
                Some(found) if found == close => {
                    self.advance_char();
                    break;
                }
                Some('}' | ']') | None => return Err(LayoutParseError::UnbalancedGroup),
                Some(found) => {
                    return Err(LayoutParseError::UnexpectedChar {
                        expected: close,
                        pos: self.pos,
                        found,
                    });
                }
            }
        }

        Ok(children)
    }

    fn parse_u16(&mut self, field: &'static str) -> Result<u16, LayoutParseError> {
        let value = self.parse_digits(field)?;
        value
            .parse::<u16>()
            .map_err(|source| LayoutParseError::InvalidInt { field, source })
    }

    fn parse_u64(&mut self, field: &'static str) -> Result<u64, LayoutParseError> {
        let value = self.parse_digits(field)?;
        value
            .parse::<u64>()
            .map_err(|source| LayoutParseError::InvalidInt { field, source })
    }

    fn parse_digits(&mut self, field: &'static str) -> Result<&'a str, LayoutParseError> {
        let start = self.pos;
        while let Some(ch) = self.peek_char() {
            if ch.is_ascii_digit() {
                self.advance_char();
            } else {
                break;
            }
        }

        let value = self
            .input
            .get(start..self.pos)
            .ok_or(LayoutParseError::UnexpectedEnd { context: "integer" })?;
        if value.is_empty() {
            if self.peek_char().is_none() {
                return Err(LayoutParseError::UnexpectedEnd { context: field });
            }
            return value
                .parse::<u16>()
                .map(|_| value)
                .map_err(|source| LayoutParseError::InvalidInt { field, source });
        }

        Ok(value)
    }

    fn expect_char(&mut self, expected: char) -> Result<(), LayoutParseError> {
        match self.peek_char() {
            Some(found) if found == expected => {
                self.advance_char();
                Ok(())
            }
            Some(found) => Err(LayoutParseError::UnexpectedChar {
                expected,
                pos: self.pos,
                found,
            }),
            None => Err(LayoutParseError::UnexpectedEnd {
                context: "character",
            }),
        }
    }

    fn peek_char(&self) -> Option<char> {
        self.input.get(self.pos..)?.chars().next()
    }

    fn advance_char(&mut self) -> Option<char> {
        let ch = self.peek_char()?;
        self.pos += ch.len_utf8();
        Some(ch)
    }

    fn is_done(&self) -> bool {
        self.pos == self.input.len()
    }

    fn remaining(&self) -> &'a str {
        self.input.get(self.pos..).unwrap_or("")
    }
}

#[cfg(test)]
fn render_layout(ast: &LayoutAst) -> String {
    let mut body = String::new();
    render_node(ast, &mut body);
    let checksum = layout_checksum(body.as_bytes());
    format!("{checksum:04x},{body}")
}

#[cfg(test)]
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

#[cfg(test)]
fn render_children(children: &[LayoutAst], out: &mut String) {
    for (idx, child) in children.iter().enumerate() {
        if idx > 0 {
            out.push(',');
        }
        render_node(child, out);
    }
}

#[cfg(test)]
fn push_rect(rect: Rect, out: &mut String) {
    out.push_str(&rect.w.to_string());
    out.push('x');
    out.push_str(&rect.h.to_string());
    out.push(',');
    out.push_str(&rect.x.to_string());
    out.push(',');
    out.push_str(&rect.y.to_string());
}

#[cfg(test)]
mod tests {
    use super::{layout_checksum, parse_layout, render_layout, LayoutAst, Rect};

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect { x, y, w, h }
    }

    fn leaf(pane_id: u64, rect: Rect) -> LayoutAst {
        LayoutAst::Leaf { pane_id, rect }
    }

    #[test]
    fn parses_single_pane() {
        let ast = leaf(0, rect(0, 0, 80, 24));
        let parsed = must_parse(&render_layout(&ast));

        assert_eq!(parsed, ast);
    }

    #[test]
    fn parses_horizontal_split() {
        let ast = LayoutAst::Horizontal {
            rect: rect(0, 0, 80, 24),
            children: vec![leaf(0, rect(0, 0, 40, 24)), leaf(1, rect(40, 0, 40, 24))],
        };
        let parsed = must_parse(&render_layout(&ast));

        assert_eq!(parsed, ast);
        if let LayoutAst::Horizontal { children, .. } = parsed {
            assert_eq!(children.len(), 2);
            assert_eq!(children[0], leaf(0, rect(0, 0, 40, 24)));
            assert_eq!(children[1], leaf(1, rect(40, 0, 40, 24)));
        } else {
            panic!("expected horizontal layout");
        }
    }

    #[test]
    fn parses_vertical_split() {
        let ast = LayoutAst::Vertical {
            rect: rect(0, 0, 80, 24),
            children: vec![leaf(0, rect(0, 0, 80, 12)), leaf(1, rect(0, 12, 80, 12))],
        };
        let parsed = must_parse(&render_layout(&ast));

        assert_eq!(parsed, ast);
    }

    #[test]
    fn parses_nested_four_pane_grid() {
        let ast = LayoutAst::Horizontal {
            rect: rect(0, 0, 80, 24),
            children: vec![
                LayoutAst::Vertical {
                    rect: rect(0, 0, 40, 24),
                    children: vec![leaf(0, rect(0, 0, 40, 12)), leaf(1, rect(0, 12, 40, 12))],
                },
                LayoutAst::Vertical {
                    rect: rect(40, 0, 40, 24),
                    children: vec![leaf(2, rect(40, 0, 40, 12)), leaf(3, rect(40, 12, 40, 12))],
                },
            ],
        };
        let parsed = must_parse(&render_layout(&ast));

        assert_eq!(parsed, ast);
    }

    #[test]
    fn pathological_inputs_return_errors() {
        let valid = render_layout(&leaf(0, rect(0, 0, 80, 24)));
        let Some((_, body)) = valid.split_once(',') else {
            panic!("rendered layout should include checksum separator");
        };
        let wrong_checksum = format!("0000,{body}");
        let valid_with_garbage = format!("{valid}garbage");
        let empty_group = layout_with_body("80x24,0,0{}");
        let truncated_mid_group = layout_with_body("80x24,0,0{40x24");
        let unbalanced_group = layout_with_body("80x24,0,0{40x24,0,0,0");
        let non_numeric_width = layout_with_body("ax24,0,0,0");

        let cases = [
            "",
            "80x24,0,0,0",
            wrong_checksum.as_str(),
            truncated_mid_group.as_str(),
            unbalanced_group.as_str(),
            valid_with_garbage.as_str(),
            empty_group.as_str(),
            non_numeric_width.as_str(),
        ];

        for case in cases {
            assert!(parse_layout(case).is_err(), "{case:?} should fail");
        }
    }

    #[test]
    fn curated_bad_inputs_do_not_panic() {
        let valid = render_layout(&leaf(0, rect(0, 0, 80, 24)));
        let Some((_, body)) = valid.split_once(',') else {
            panic!("rendered layout should include checksum separator");
        };
        let wrong_checksum = format!("0000,{body}");
        let empty_group = layout_with_body("80x24,0,0[]");
        let trailing = format!("{valid}\u{2603}");

        let cases = [
            "",
            "not-layout",
            "abcd,",
            "abcd,80x",
            "abcd,80x24,0,0{",
            "abcd,80x24,0,0]",
            "ffff,\u{2603}",
            wrong_checksum.as_str(),
            empty_group.as_str(),
            trailing.as_str(),
        ];

        for case in cases {
            assert!(parse_layout(case).is_err(), "{case:?} should fail");
        }
    }

    fn layout_with_body(body: &str) -> String {
        format!("{:04x},{body}", layout_checksum(body.as_bytes()))
    }

    fn must_parse(input: &str) -> LayoutAst {
        match parse_layout(input) {
            Ok(ast) => ast,
            Err(error) => {
                panic!("layout should parse: {error}");
            }
        }
    }
}

#[cfg(test)]
mod proptests {
    use super::{parse_layout, render_layout, LayoutAst, Rect};
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: 1024,
            ..ProptestConfig::default()
        })]

        #[test]
        fn roundtrip(ast in arbitrary_layout()) {
            let rendered = render_layout(&ast);
            let parsed = parse_layout(&rendered).expect("rendered output must parse");

            prop_assert_eq!(parsed, ast);
        }

        #[test]
        fn arbitrary_strings_never_panic(s in any::<String>()) {
            let _ = parse_layout(&s);
        }

        #[test]
        fn arbitrary_ascii_never_panic(s in r"[\x00-\x7f]{0,256}") {
            let _ = parse_layout(&s);
        }
    }

    fn arbitrary_layout() -> impl Strategy<Value = LayoutAst> {
        arbitrary_leaf().prop_recursive(4, 64, 4, |inner| {
            (
                arbitrary_rect(),
                prop::collection::vec(inner, 2..=4),
                any::<bool>(),
            )
                .prop_map(|(rect, children, horizontal)| {
                    if horizontal {
                        LayoutAst::Horizontal { rect, children }
                    } else {
                        LayoutAst::Vertical { rect, children }
                    }
                })
        })
    }

    fn arbitrary_leaf() -> impl Strategy<Value = LayoutAst> {
        (any::<u64>(), arbitrary_rect())
            .prop_map(|(pane_id, rect)| LayoutAst::Leaf { pane_id, rect })
    }

    fn arbitrary_rect() -> impl Strategy<Value = Rect> {
        (0u16..=200, 0u16..=200, 1u16..=200, 1u16..=200).prop_map(|(x, y, w, h)| Rect {
            x,
            y,
            w,
            h,
        })
    }
}
