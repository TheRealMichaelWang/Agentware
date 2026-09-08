//! Naming cells: `B7`, `A1:C5`, and the arithmetic between them.
//!
//! A spreadsheet's cells are coordinates rather than nodes, so every process
//! that talks about one talks in this notation: the haimanager holds sheets
//! and paints them by it, an application publishes runs at a cell named by
//! it, and the agent reads rectangles and hears which rectangles changed. It
//! lives in the protocol crate for the same reason the wire formats do: three
//! ends speak it, and one copy is what keeps them agreeing.
//!
//! The lettering is the spreadsheet's own, base 26 with no zero: A to Z, then
//! AA. It is not the application's to choose because a spreadsheet's columns
//! are named this way everywhere and an application that had to say so would
//! only ever say the same thing.

/// A cell's coordinates: column then row, both counted from zero.
///
/// Numbers rather than the `B7` an application writes, because painting a
/// screenful means walking a rectangle of them and arithmetic is what a
/// rectangle is made of.
pub type Ref = (u32, u32);

/// A rectangle of cells, as its two corners: the top left and the bottom
/// right, inclusive. Built by [`parse_range`] and [`union`], which both keep
/// the corners in that order.
pub type Range = (Ref, Ref);

/// Turn `B7` into the coordinates it names, or `None` when it is not a
/// reference at all.
pub fn parse(at: &str) -> Option<Ref> {
    let letters = at.len() - at.trim_start_matches(|c: char| c.is_ascii_uppercase()).len();
    if letters == 0 || letters > 4 {
        return None;
    }
    let (name, number) = at.split_at(letters);

    let mut column: u32 = 0;
    for letter in name.bytes() {
        column = column.checked_mul(26)?.checked_add((letter - b'A' + 1) as u32)?;
    }
    let row: u32 = number.parse().ok()?;
    if row == 0 {
        return None;
    }
    Some((column - 1, row - 1))
}

/// The name a column has, counting from zero: 0 is A, 25 is Z, 26 is AA.
pub fn column_name(mut column: u32) -> String {
    let mut name = Vec::new();
    loop {
        name.push(b'A' + (column % 26) as u8);
        if column < 26 {
            break;
        }
        column = column / 26 - 1;
    }
    name.reverse();
    String::from_utf8(name).unwrap_or_default()
}

/// The name a cell has: `B7`.
pub fn name(at: Ref) -> String {
    format!("{}{}", column_name(at.0), at.1 + 1)
}

/// The two corners of `A1:C5`, or of a single `B7`.
pub fn parse_range(text: &str) -> Option<Range> {
    match text.split_once(':') {
        Some((from, to)) => {
            let (from, to) = (parse(from)?, parse(to)?);
            Some((
                (from.0.min(to.0), from.1.min(to.1)),
                (from.0.max(to.0), from.1.max(to.1)),
            ))
        }
        None => {
            let one = parse(text)?;
            Some((one, one))
        }
    }
}

/// The name a rectangle has: `A1:C5`, or `B7` for a rectangle of one cell,
/// which is what a person would write and what [`parse_range`] reads back.
pub fn range_name((from, to): Range) -> String {
    if from == to { name(from) } else { format!("{}:{}", name(from), name(to)) }
}

/// The smallest rectangle holding both.
///
/// What a run of changes adds up to: three runs into a row and one into the
/// row below are one rectangle, and one rectangle is one thing to read.
pub fn union(a: Range, b: Range) -> Range {
    (
        (a.0.0.min(b.0.0), a.0.1.min(b.0.1)),
        (a.1.0.max(b.1.0), a.1.1.max(b.1.1)),
    )
}

/// How many cells a rectangle holds. Saturating, because the axes are capped
/// at a hundred thousand each and their product is not a number anyone wants
/// to read cells up to.
pub fn count((from, to): Range) -> u64 {
    let width = u64::from(to.0 - from.0) + 1;
    let height = u64::from(to.1 - from.1) + 1;
    width.saturating_mul(height)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The lettering is base 26 with no zero, which is the one part of this
    /// that is easy to write wrong: 26 is AA, not BA, because there is no
    /// column zero to carry into.
    #[test]
    fn columns_are_lettered_the_way_a_spreadsheet_letters_them() {
        for (column, expected) in
            [(0, "A"), (25, "Z"), (26, "AA"), (27, "AB"), (51, "AZ"), (52, "BA")]
        {
            assert_eq!(column_name(column), expected, "column {column}");
            assert_eq!(parse(&format!("{expected}1")), Some((column, 0)), "{expected}1");
        }
        assert_eq!(parse("B7"), Some((1, 6)));
        assert_eq!(name((1, 6)), "B7");
        // Not references.
        assert_eq!(parse("7"), None);
        assert_eq!(parse("B"), None);
        assert_eq!(parse("B0"), None);
        assert_eq!(parse("b7"), None);
    }

    /// A range is read in whatever order its corners were written and comes
    /// out top left first, so everything downstream can assume the order.
    #[test]
    fn ranges_are_normalised_and_named_back() {
        assert_eq!(parse_range("C5:A1"), Some(((0, 0), (2, 4))));
        assert_eq!(parse_range("B7"), Some(((1, 6), (1, 6))));
        assert_eq!(parse_range("B7:x"), None);
        assert_eq!(range_name(((0, 0), (2, 4))), "A1:C5");
        // One cell is named as one cell, the way a person writes it.
        assert_eq!(range_name(((1, 6), (1, 6))), "B7");
    }

    /// Three runs into a row and one into the row below are one rectangle,
    /// which is one thing for an agent to read rather than four.
    #[test]
    fn a_union_is_the_smallest_rectangle_holding_both() {
        let row = union(((1, 6), (1, 6)), ((3, 6), (3, 6)));
        assert_eq!(row, ((1, 6), (3, 6)));
        assert_eq!(union(row, ((0, 7), (0, 7))), ((0, 6), (3, 7)));
        assert_eq!(count(((0, 6), (3, 7))), 8);
        assert_eq!(count(((1, 6), (1, 6))), 1);
    }
}
