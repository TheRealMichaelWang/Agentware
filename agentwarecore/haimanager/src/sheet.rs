//! The sheets an application keeps behind its `spreadsheet` elements.
//!
//! A spreadsheet is the one element whose content does not live in the tree.
//! Everything else in Agentware is described by resending the whole document
//! and letting the compositor diff it, which works because everything else is
//! small. A sheet is not: describing ten thousand cells as ten thousand
//! elements costs an agent forty kilobytes to read one screen, and costs the
//! application a re-serialisation of the visible grid on every keystroke.
//!
//! So the tree carries a *name* and a *version*, the way an `image` carries a
//! path, and the cells arrive on the same socket as their own kind of frame.
//! The compositor holds the sheet and paints from it directly, with no nodes,
//! no rectangles and no diff: it does not work out what changed, it is told.
//!
//! Ordering is free because it is one connection. An application sends the
//! cells that make a version and then the tree that claims it, so the
//! compositor can never hold a tree ahead of its data. The version in the tree
//! is what makes divergence impossible to sit on: every single render
//! re-asserts which version the picture is of, so a sheet that fell behind is
//! caught on the next frame rather than drifting.

use std::collections::HashMap;

/// A cell's coordinates: column then row, both counted from zero.
///
/// Kept as numbers rather than as the `B7` an application writes, because
/// painting a screenful means walking a rectangle of them and arithmetic is
/// what a rectangle is made of.
pub type Ref = (u32, u32);

/// One application's sheet.
#[derive(Default)]
pub struct Sheet {
    /// The cells that have something in them. Sparse, because a sheet is:
    /// a thousand rows of nothing cost nothing.
    cells: HashMap<Ref, String>,
    /// What the application says the sheet's version is, after everything
    /// applied so far.
    version: u64,
}

impl Sheet {
    pub fn version(&self) -> u64 {
        self.version
    }

    pub fn get(&self, at: Ref) -> Option<&str> {
        self.cells.get(&at).map(String::as_str)
    }

    /// The furthest cell that has anything in it, for telling an agent where
    /// the sheet actually stops. `None` when it is empty.
    pub fn used(&self) -> Option<(Ref, Ref)> {
        let mut bounds: Option<(Ref, Ref)> = None;
        for &(column, row) in self.cells.keys() {
            bounds = Some(match bounds {
                None => ((column, row), (column, row)),
                Some(((x0, y0), (x1, y1))) => {
                    ((x0.min(column), y0.min(row)), (x1.max(column), y1.max(row)))
                }
            });
        }
        bounds
    }

    /// Put a run of values in, starting at `at` and running across.
    ///
    /// Returns whether it applied. A base that is not the version held means
    /// the compositor missed something, and the only honest answer is to ask
    /// for the sheet from the beginning rather than to write cells into a
    /// picture that is already wrong.
    pub fn apply(&mut self, base: u64, version: u64, at: Ref, values: &[String]) -> bool {
        if base == 0 {
            // A snapshot: forget what was here. One code path for the first
            // frame and for a recovery, because they are the same thing.
            self.cells.clear();
        } else if base != self.version {
            return false;
        }

        let (column, row) = at;
        for (step, value) in values.iter().enumerate() {
            let at = (column + step as u32, row);
            // An empty value is an empty cell, and an empty cell is one that
            // is not there: a sheet stays sparse however much is cleared.
            if value.is_empty() {
                self.cells.remove(&at);
            } else {
                self.cells.insert(at, value.clone());
            }
        }
        self.version = version;
        true
    }
}

/// Turn `B7` into the coordinates it names, or `None` when it is not a
/// reference at all.
///
/// The lettering is the spreadsheet's own, base 26 with no zero: A to Z, then
/// AA. It is the compositor's rather than the application's because a
/// spreadsheet's columns are named this way everywhere and an application that
/// had to say so would only ever say the same thing.
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
pub fn parse_range(text: &str) -> Option<(Ref, Ref)> {
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

    /// A run applies to the version it says it applies to, and to no other.
    /// The whole of the protocol's safety is this one comparison: a delta that
    /// cannot be placed is refused rather than written into a picture that is
    /// already wrong.
    #[test]
    fn a_run_applies_only_to_the_version_it_names() {
        let mut sheet = Sheet::default();
        // A snapshot starts from nothing, whatever was there.
        assert!(sheet.apply(0, 1, (0, 0), &["Region".into(), "Q1".into(), "Q2".into()]));
        assert_eq!(sheet.get((0, 0)), Some("Region"));
        assert_eq!(sheet.get((2, 0)), Some("Q2"));
        assert_eq!(sheet.version(), 1);

        // A delta on the version held.
        assert!(sheet.apply(1, 2, (1, 6), &["4711".into()]));
        assert_eq!(sheet.get((1, 6)), Some("4711"));

        // One that names a version nobody has is refused, and changes nothing.
        assert!(!sheet.apply(9, 10, (1, 6), &["wrong".into()]));
        assert_eq!(sheet.get((1, 6)), Some("4711"));
        assert_eq!(sheet.version(), 2);

        // An empty value empties the cell rather than storing emptiness.
        assert!(sheet.apply(2, 3, (1, 6), &[String::new()]));
        assert_eq!(sheet.get((1, 6)), None);
        assert_eq!(sheet.used(), Some(((0, 0), (2, 0))));
    }
}

/// Every sheet one client holds, by the name its element points at.
///
/// One application can have several: a workbook with three tabs is three
/// sources, and two elements may point at the same one if an application ever
/// wants two views of a sheet.
#[derive(Default)]
pub struct Sheets {
    by_source: HashMap<String, Sheet>,
}

impl Sheets {
    pub fn get(&self, source: &str) -> Option<&Sheet> {
        self.by_source.get(source)
    }

    /// Apply a run, creating the sheet if this is the first thing said about
    /// it. Returns whether it applied; a false means the caller should ask for
    /// the sheet from the beginning.
    pub fn apply(&mut self, source: &str, base: u64, version: u64, at: Ref, values: &[String]) -> bool {
        let sheet = self.by_source.entry(source.to_owned()).or_default();
        sheet.apply(base, version, at, values)
    }

    /// The version held for a source, zero for one nothing has been said
    /// about. What `sheet-resend` carries, so an application knows how far
    /// behind the picture is.
    pub fn version(&self, source: &str) -> u64 {
        self.by_source.get(source).map_or(0, Sheet::version)
    }

}
