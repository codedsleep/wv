//! tmux format strings: `#{pane_id}`, `#S`, `#{?flag,yes,no}`.
//!
//! A format string is what makes `list-panes` and `display-message` useful to a
//! script — it decides what comes back, so the caller does not have to parse
//! prose. This module does the expanding; the variables themselves are
//! assembled by whoever knows the session.
//!
//! What is supported:
//!
//! | Form | Meaning |
//! |---|---|
//! | `#{name}` | the variable `name`, or empty if unknown |
//! | `#{?name,then,else}` | `then` when `name` is true, `else` otherwise |
//! | `#X` | short alias, e.g. `#S` for `session_name` |
//! | `##` | a literal `#` |
//!
//! A variable is "true" when it is neither empty nor `"0"`, which is tmux's
//! rule and what makes `#{?pane_active,*,}` work against a flag of `1`/`0`.
//!
//! Not supported, and rejected rather than silently mis-expanded: arithmetic,
//! comparisons, substitution and the `#{==:...}` family.

use std::collections::HashMap;

/// The variables one row of output can refer to.
#[derive(Clone, Debug, Default)]
pub struct Variables {
    values: HashMap<&'static str, String>,
}

impl Variables {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&mut self, name: &'static str, value: impl Into<String>) -> &mut Self {
        self.values.insert(name, value.into());
        self
    }

    /// Set a boolean the tmux way: `1` or `0`, so `#{?flag,..}` reads it.
    pub fn set_flag(&mut self, name: &'static str, value: bool) -> &mut Self {
        self.set(name, if value { "1" } else { "0" })
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.values.get(name).map(String::as_str)
    }

    fn is_true(&self, name: &str) -> bool {
        !matches!(self.get(name), None | Some("" | "0"))
    }
}

/// Why a format string could not be expanded.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum FormatError {
    #[error("unterminated `#{{` in format string")]
    Unterminated,
    #[error("`#{{{0}}}` is not supported: weave has no format arithmetic or comparisons")]
    Unsupported(String),
    #[error("`#{{?{0}}}` needs at least a condition and a true branch")]
    MalformedConditional(String),
}

/// Expand `template` against `vars`.
pub fn expand(template: &str, vars: &Variables) -> Result<String, FormatError> {
    let mut out = String::with_capacity(template.len());
    let mut chars = template.char_indices().peekable();

    while let Some((index, ch)) = chars.next() {
        if ch != '#' {
            out.push(ch);
            continue;
        }

        match chars.peek().map(|(_, ch)| *ch) {
            // `##` is a literal hash.
            Some('#') => {
                chars.next();
                out.push('#');
            }
            Some('{') => {
                let body = read_braced(template, index)?;
                // Skip what `read_braced` consumed: `#{` + body + `}`.
                for _ in 0..body.chars().count() + 2 {
                    chars.next();
                }
                out.push_str(&expand_braced(&body, vars)?);
            }
            // A `#` before anything that is not an alias is just a `#`,
            // as in tmux.
            Some(short) => match short_alias(short) {
                Some(name) => {
                    chars.next();
                    out.push_str(vars.get(name).unwrap_or_default());
                }
                None => out.push('#'),
            },
            None => out.push('#'),
        }
    }

    Ok(out)
}

/// Read the body of a `#{...}` starting at the `#`, without the braces.
fn read_braced(template: &str, start: usize) -> Result<String, FormatError> {
    let after_brace = start + "#{".len();
    let rest = template.get(after_brace..).ok_or(FormatError::Unterminated)?;

    let mut depth = 1usize;
    for (offset, ch) in rest.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(rest[..offset].to_owned());
                }
            }
            _ => {}
        }
    }

    Err(FormatError::Unterminated)
}

fn expand_braced(body: &str, vars: &Variables) -> Result<String, FormatError> {
    if let Some(conditional) = body.strip_prefix('?') {
        return expand_conditional(conditional, vars);
    }

    // Anything with an operator in it belongs to the parts of tmux's format
    // language weave does not implement. Say so rather than expanding to "".
    if body.starts_with('=') || body.contains(':') || body.starts_with('!') {
        return Err(FormatError::Unsupported(body.to_owned()));
    }

    Ok(vars.get(body).unwrap_or_default().to_owned())
}

/// `?cond,then,else` — the comma split respects nested `#{...}`.
fn expand_conditional(body: &str, vars: &Variables) -> Result<String, FormatError> {
    let parts = split_top_level(body);
    let [condition, then_branch, else_branch] = match parts.as_slice() {
        [condition, then_branch] => [condition.clone(), then_branch.clone(), String::new()],
        [condition, then_branch, else_branch] => [
            condition.clone(),
            then_branch.clone(),
            else_branch.clone(),
        ],
        _ => return Err(FormatError::MalformedConditional(body.to_owned())),
    };

    let taken = if vars.is_true(&condition) {
        then_branch
    } else {
        else_branch
    };

    // Branches may themselves contain formats.
    expand(&taken, vars)
}

/// Split on commas that are not inside a nested `#{...}`.
fn split_top_level(body: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;

    for ch in body.chars() {
        match ch {
            '{' => {
                depth += 1;
                current.push(ch);
            }
            '}' => {
                depth = depth.saturating_sub(1);
                current.push(ch);
            }
            ',' if depth == 0 => parts.push(std::mem::take(&mut current)),
            _ => current.push(ch),
        }
    }
    parts.push(current);

    parts
}

/// tmux's one-letter shorthands.
const fn short_alias(ch: char) -> Option<&'static str> {
    Some(match ch {
        'S' => "session_name",
        'W' => "window_name",
        'I' => "window_index",
        'P' => "pane_index",
        'D' => "pane_id",
        'T' => "pane_title",
        'F' => "window_flags",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::{expand, FormatError, Variables};

    fn vars() -> Variables {
        let mut vars = Variables::new();
        vars.set("session_name", "dev")
            .set("window_index", "2")
            .set("window_name", "build")
            .set("pane_id", "%3")
            .set("pane_title", "vim")
            .set_flag("pane_active", true)
            .set_flag("pane_dead", false);
        vars
    }

    fn expanded(template: &str) -> String {
        expand(template, &vars()).expect("format expands")
    }

    #[test]
    fn substitutes_named_variables() {
        assert_eq!(expanded("#{session_name}:#{window_index}"), "dev:2");
        assert_eq!(expanded("#{pane_id}"), "%3");
    }

    #[test]
    fn unknown_variables_expand_to_nothing() {
        assert_eq!(expanded("[#{nope}]"), "[]");
    }

    #[test]
    fn short_aliases_match_their_long_names() {
        assert_eq!(expanded("#S"), expanded("#{session_name}"));
        assert_eq!(expanded("#I"), "2");
        assert_eq!(expanded("#W"), "build");
        assert_eq!(expanded("#D"), "%3");
    }

    #[test]
    fn double_hash_is_a_literal_hash() {
        assert_eq!(expanded("##{not_a_var}"), "#{not_a_var}");
        assert_eq!(expanded("a##b"), "a#b");
    }

    #[test]
    fn a_lone_hash_survives() {
        assert_eq!(expanded("100# "), "100# ");
    }

    /// `#{?flag,*,}` against a `1`/`0` flag is the idiom that marks the active
    /// pane in a listing, so it gets its own test.
    #[test]
    fn conditionals_read_flags() {
        assert_eq!(expanded("#{?pane_active,*,-}"), "*");
        assert_eq!(expanded("#{?pane_dead,dead,live}"), "live");
        assert_eq!(expanded("#{?pane_active,*,}"), "*");
        assert_eq!(expanded("#{?pane_dead,dead,}"), "");
    }

    #[test]
    fn an_unset_variable_is_false() {
        assert_eq!(expanded("#{?nope,yes,no}"), "no");
    }

    #[test]
    fn conditional_branches_expand_too() {
        assert_eq!(expanded("#{?pane_active,#{pane_id},none}"), "%3");
    }

    #[test]
    fn nested_braces_do_not_break_the_comma_split() {
        assert_eq!(
            expanded("#{?pane_active,#{window_name},#{session_name}}"),
            "build"
        );
    }

    #[test]
    fn unterminated_formats_are_an_error() {
        assert_eq!(
            expand("#{pane_id", &vars()),
            Err(FormatError::Unterminated)
        );
    }

    /// Arithmetic and comparisons expand to something plausible-looking if
    /// ignored, which would quietly corrupt a script's output.
    #[test]
    fn unsupported_format_operators_are_refused() {
        assert!(matches!(
            expand("#{==:a,b}", &vars()),
            Err(FormatError::Unsupported(_))
        ));
        assert!(matches!(
            expand("#{e|+|:1,2}", &vars()),
            Err(FormatError::Unsupported(_))
        ));
    }

    #[test]
    fn a_malformed_conditional_is_refused() {
        assert!(matches!(
            expand("#{?onlycond}", &vars()),
            Err(FormatError::MalformedConditional(_))
        ));
    }
}
