//! The one file format this understands.
//!
//! Deliberately the whole of it: fields separated by commas, rows by newlines,
//! and a field that contains any of those wrapped in quotes with its own
//! quotes doubled. That is what every spreadsheet writes and what every one of
//! them reads, and a sheet of text has nothing else to say.
//!
//! Reading is forgiving because a file comes from somewhere else: rows may be
//! ragged, line endings may be either kind, and a file that ends without a
//! newline still has a last row. Writing is strict, because what goes out is
//! ours.

/// Split CSV text into rows of fields.
pub fn parse(text: &str) -> Vec<Vec<String>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    // Whether anything at all has been seen since the last newline. It is what
    // tells a file ending in a newline from one ending in a last row, so a
    // round trip does not grow a blank row every time it is saved.
    let mut started = false;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        started = true;
        if quoted {
            if c == '"' {
                // A doubled quote is a quote. A single one ends the field, and
                // anything after it before the comma is junk we keep, since
                // refusing a file over it helps nobody.
                if chars.peek() == Some(&'"') {
                    chars.next();
                    field.push('"');
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            // Quotes only open a field at its start; one in the middle is a
            // character, which is what a spreadsheet writing 5" would produce.
            '"' if field.is_empty() => quoted = true,
            ',' => row.push(std::mem::take(&mut field)),
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
                started = false;
            }
            // Both line endings, without caring which: a lone carriage return
            // before a newline is the other convention, and one on its own is
            // nothing worth keeping in a cell.
            '\r' => {}
            _ => field.push(c),
        }
    }
    if started {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// Write rows out, quoting the fields that need it.
pub fn write(rows: &[Vec<String>]) -> String {
    let mut out = String::new();
    for row in rows {
        for (at, field) in row.iter().enumerate() {
            if at > 0 {
                out.push(',');
            }
            push_field(&mut out, field);
        }
        out.push('\n');
    }
    out
}

fn push_field(out: &mut String, field: &str) {
    if !field.contains([',', '"', '\n', '\r']) {
        out.push_str(field);
        return;
    }
    out.push('"');
    for c in field.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same rows, written the way a test can read them.
    fn rows(text: &str) -> Vec<Vec<String>> {
        parse(text)
    }

    fn like(lines: &[&[&str]]) -> Vec<Vec<String>> {
        lines.iter().map(|row| row.iter().map(|f| (*f).to_owned()).collect()).collect()
    }

    #[test]
    fn plain_rows_split_on_commas_and_newlines() {
        assert_eq!(rows("a,b\nc,d\n"), like(&[&["a", "b"], &["c", "d"]]));
        // A file that ends without a newline still has its last row, and one
        // that ends with a newline does not grow an empty one.
        assert_eq!(rows("a,b\nc,d"), like(&[&["a", "b"], &["c", "d"]]));
        assert_eq!(rows("a,b\r\nc,d\r\n"), like(&[&["a", "b"], &["c", "d"]]));
        assert_eq!(rows(""), like(&[]));
        // Ragged is fine: a sheet is a rectangle, a file need not be.
        assert_eq!(rows("a\nb,c,d\n"), like(&[&["a"], &["b", "c", "d"]]));
        // Empty fields are cells, not absences.
        assert_eq!(rows("a,,c\n"), like(&[&["a", "", "c"]]));
    }

    #[test]
    fn quotes_carry_the_characters_that_would_otherwise_split_a_row() {
        assert_eq!(rows("\"a,b\",c\n"), like(&[&["a,b", "c"]]));
        assert_eq!(rows("\"say \"\"hi\"\"\",c\n"), like(&[&["say \"hi\"", "c"]]));
        assert_eq!(rows("\"two\nlines\",c\n"), like(&[&["two\nlines", "c"]]));
        // Not every quote opens a field: one after a character is a character,
        // which is how a measurement in inches survives.
        assert_eq!(rows("5\" pipe,c\n"), like(&[&["5\" pipe", "c"]]));
    }

    /// The property that matters: anything this writes, it reads back.
    #[test]
    fn what_is_written_is_read_back_unchanged() {
        let original = vec![
            vec!["Region".to_owned(), "Q1, adjusted".to_owned(), String::new()],
            vec!["North \"best\"".to_owned(), "1240".to_owned(), "two\nlines".to_owned()],
            vec![String::new(), String::new(), "-".to_owned()],
        ];
        assert_eq!(parse(&write(&original)), original);
        // And the shape of what it writes is the ordinary one, so another
        // program reads it too.
        assert_eq!(
            write(&[vec!["a".to_owned(), "b,c".to_owned()]]),
            "a,\"b,c\"\n"
        );
    }
}
