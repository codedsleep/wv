//! Shared parser for tmux `-F` pipe-delimited rows.
//!
//! tmux format strings can carry arbitrary characters (spaces in window names,
//! brackets in layouts, etc.). Pipe-delimited output (`#{a}|#{b}|#{c}`) is
//! cheap to parse and the only failure mode is a pipe inside a field's contents,
//! which is rare and only affects the last field thanks to `splitn`.

/// Parse pipe-delimited rows from a tmux `-F` query.
///
/// Each line is split into exactly `expected_fields` columns. Lines with fewer
/// columns are skipped. The last column may legitimately contain `|` because
/// `splitn` stops after `expected_fields - 1` splits.
pub fn parse_rows(output: &str, expected_fields: usize) -> Vec<Vec<&str>> {
    output
        .lines()
        .filter_map(|line| {
            let row: Vec<&str> = line.splitn(expected_fields, '|').collect();
            (row.len() == expected_fields).then_some(row)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::parse_rows;

    #[test]
    fn parses_three_rows_with_three_fields() {
        let input = "a|b|c\nd|e|f\ng|h|i\n";
        let rows = parse_rows(input, 3);

        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0], vec!["a", "b", "c"]);
        assert_eq!(rows[2], vec!["g", "h", "i"]);
    }

    #[test]
    fn last_field_keeps_internal_pipes() {
        let input = "ok|hello|pipe|name|with|pipes\n";
        let rows = parse_rows(input, 3);

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0][2], "pipe|name|with|pipes");
    }

    #[test]
    fn drops_rows_with_too_few_fields() {
        let input = "a|b|c\nbad-row\nd|e|f\n";
        let rows = parse_rows(input, 3);

        assert_eq!(rows.len(), 2);
    }

    #[test]
    fn empty_input_yields_no_rows() {
        assert!(parse_rows("", 2).is_empty());
        assert!(parse_rows("\n\n", 2).is_empty());
    }
}
