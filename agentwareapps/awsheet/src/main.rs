//! A grid, to try the spreadsheet element against.
//!
//! Not the spreadsheet application: there are no formulas, no files and no
//! arithmetic. It is the smallest thing that exercises what the compositor
//! offers, so the element can be used before anything is built on it, and it
//! is the reference for how an application drives one.
//!
//! The shape to notice is that **the cells are not in the tree**. Everything
//! else an application draws is described by resending the whole document and
//! letting the compositor diff it, which works because everything else is
//! small. A sheet is not, so it goes the other way: the cells are published as
//! runs on their own frames, the tree carries the sheet's *name* and the
//! *version* it was drawn against, and the compositor holds the sheet and
//! paints from it. Nothing is compared with anything. This says
//! `put("B7", &["4711"])` and that is the entire message.
//!
//! The version is what ties the two together, and `SheetOut` owns it so this
//! cannot get it wrong. Cells first, then the tree that claims them: one
//! connection, so they arrive in that order.
//!
//! Everything the human does still arrives as ordinary events, with the cell
//! named beside the action, because a cell is a coordinate rather than a node.
//! What is chosen and where the cursor is are this application's state, sent
//! on the element, exactly as a field's value is.
//!
//! Usage: awsheet

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write as IoWrite;

use awproto::display::{self, Event, SheetOut, Surface, escape};

/// How far the grid runs. Both are numbers the element declares; nothing is
/// described until something is put in it.
const ROWS: u32 = 1000;
const COLUMNS: u32 = 26;

struct Book {
    /// What has been typed, by sheet name and cell. Absent is empty, so a
    /// thousand rows of nothing cost nothing here as well.
    cells: HashMap<(String, Ref), String>,
    /// Which sheet is on screen. The tabs choose it; the cells are kept per
    /// sheet, so switching is not a way of losing work.
    sheet: usize,
    /// The sheets there are, each with a number that is only ever its own.
    sheets: Vec<(u32, String)>,
    next_sheet: u32,
    /// The cell the cursor is on, and the run chosen, if there is one. Both
    /// are this application's: they arrive as events and go back on the
    /// element, which is what lets the compositor draw them.
    cursor: Ref,
    range: Option<(Ref, Ref)>,
    /// The stream the cells are published on, and the version it is up to.
    out: SheetOut,
    /// Whether the menu is showing its items.
    menu_open: bool,
    /// The last thing that happened, so the window's own behaviour is visible
    /// while playing with it.
    note: String,
}

/// A cell's coordinates: column then row, counted from zero, exactly as the
/// compositor counts them.
type Ref = (u32, u32);

fn main() {
    let mut surface = match Surface::inherited() {
        Ok(surface) => surface,
        Err(err) => {
            log(&format!("no interface connection: {err}"));
            std::process::exit(1);
        }
    };

    let mut book = Book {
        cells: HashMap::new(),
        sheet: 0,
        sheets: vec![(1, "Sheet 1".into()), (2, "Sheet 2".into()), (3, "Sheet 3".into())],
        next_sheet: 4,
        cursor: (0, 0),
        range: None,
        out: SheetOut::new("book"),
        menu_open: false,
        note: String::new(),
    };
    book.seed();

    // Cells first, then the tree that claims their version.
    book.publish_all(&mut surface);
    if let Err(err) = surface.render(&book.render()) {
        log(&format!("could not send the first tree: {err}"));
        std::process::exit(1);
    }

    loop {
        let event = match surface.next_event() {
            Ok(Some(event)) => event,
            Ok(None) => {
                log("the compositor closed the connection");
                return;
            }
            Err(err) => {
                log(&format!("connection failed: {err}"));
                std::process::exit(1);
            }
        };

        // A sheet the compositor could not follow is published again from the
        // beginning. It asks when a run named a version it does not hold,
        // which it cannot apply without guessing.
        for source in surface.resend() {
            log(&format!("republishing {source} from the beginning"));
            book.out.restart();
            book.publish_all(&mut surface);
        }

        // The staleness check reads the surface and publishing writes to it,
        // so the one thing the surface is asked for is taken out first.
        let stale = surface.is_stale(&event);
        if !book.accept(stale, &event, &mut surface) {
            continue;
        }
        if let Err(err) = surface.render(&book.render()) {
            log(&format!("could not send a tree: {err}"));
            return;
        }
    }
}

impl Book {
    /// The sheet on screen, by name.
    fn name(&self) -> String {
        self.sheets.get(self.sheet).map_or(String::new(), |(_, name)| name.clone())
    }

    fn get(&self, at: Ref) -> &str {
        self.cells.get(&(self.name(), at)).map_or("", String::as_str)
    }

    fn put(&mut self, at: Ref, value: &str) {
        let key = (self.name(), at);
        if value.is_empty() {
            self.cells.remove(&key);
        } else {
            self.cells.insert(key, value.to_owned());
        }
    }

    /// Something to look at, and something far enough down to prove that
    /// reading past the screen costs nothing.
    fn seed(&mut self) {
        let rows: [[&str; 4]; 5] = [
            ["Region", "Q1", "Q2", "Q3"],
            ["North", "1240", "1310", "1288"],
            ["South", "980", "1105", "1170"],
            ["East", "1512", "1490", "1533"],
            ["West", "870", "920", "1004"],
        ];
        for (row, values) in rows.iter().enumerate() {
            for (column, value) in values.iter().enumerate() {
                self.put((column as u32, row as u32), value);
            }
        }
        self.put((0, 499), "row five hundred");
        self.put((1, 499), "still here");
    }

    /// Publish the whole of the sheet on screen: one run per row that has
    /// anything in it. What a snapshot is, and what a `sheet-resend` gets.
    fn publish_all(&mut self, surface: &mut Surface) {
        let name = self.name();
        let mut rows: HashMap<u32, Vec<(u32, String)>> = HashMap::new();
        for ((sheet, (column, row)), value) in &self.cells {
            if *sheet == name {
                rows.entry(*row).or_default().push((*column, value.clone()));
            }
        }
        // Sorted, so a run really is a run: the compositor is told where one
        // starts and the values that follow it across.
        let mut order: Vec<u32> = rows.keys().copied().collect();
        order.sort_unstable();
        for row in order {
            let mut line = rows.remove(&row).unwrap_or_default();
            line.sort_unstable();
            let first = line[0].0;
            // Gaps inside a row are empty values rather than another run,
            // because an empty value is an empty cell.
            let last = line[line.len() - 1].0;
            let mut values = vec![String::new(); (last - first + 1) as usize];
            for (column, value) in line {
                values[(column - first) as usize] = value;
            }
            let borrowed: Vec<&str> = values.iter().map(String::as_str).collect();
            let at = cell_name((first, row));
            if let Err(err) = self.out.put(surface, &at, &borrowed) {
                log(&format!("could not publish {at}: {err}"));
                return;
            }
        }
    }

    /// Publish one cell.
    fn publish(&mut self, surface: &mut Surface, at: Ref) {
        let value = self.get(at).to_owned();
        let name = cell_name(at);
        if let Err(err) = self.out.put(surface, &name, &[&value]) {
            log(&format!("could not publish {name}: {err}"));
        }
    }

    /// Act on an event, or say that nothing changed.
    fn accept(&mut self, stale: bool, event: &Event, out: &mut Surface) -> bool {
        if stale && event.action == display::ACTION_CLICK {
            return false;
        }

        let at = parse_cell(&event.cell);
        match event.action.as_str() {
            display::ACTION_SELECT => {
                if let Some(at) = at {
                    self.cursor = at;
                    self.range = None;
                    self.note = format!("{} chosen", cell_name(at));
                    return true;
                }
                if let Some(sheet) = self.tab_at(&event.target) {
                    self.sheet = sheet;
                    self.cursor = (0, 0);
                    self.range = None;
                    self.note = format!("{} on screen", self.sheets[sheet].1);
                    // A different sheet is a different set of cells, so the
                    // one the compositor holds is replaced whole.
                    self.out.restart();
                    self.publish_all(out);
                    return true;
                }
                false
            }

            display::ACTION_SELECT_RANGE => {
                let (Some(near), Some(far)) = (at, parse_cell(&event.value)) else {
                    return false;
                };
                self.range = Some((near, far));
                self.note = format!("{} through {}", cell_name(near), cell_name(far));
                true
            }

            display::ACTION_TYPE_TEXT => {
                let Some(at) = at else { return false };
                self.put(at, &event.value);
                self.publish(out, at);
                self.note = format!("{} is now {:?}", cell_name(at), event.value);
                true
            }

            display::ACTION_SUBMIT => at.is_some(),

            display::ACTION_CONTEXT => {
                self.menu_open = true;
                self.note = "menu".to_owned();
                true
            }

            display::ACTION_OPEN => {
                self.menu_open = true;
                true
            }

            display::ACTION_CLOSE => {
                if let Some(sheet) = self.tab_at(&event.target) {
                    if self.sheets.len() > 1 {
                        self.sheets.remove(sheet);
                        self.sheet = self.sheet.min(self.sheets.len() - 1);
                        self.cursor = (0, 0);
                        self.range = None;
                        self.out.restart();
                        self.publish_all(out);
                        self.note = "sheet closed".to_owned();
                    }
                } else {
                    self.menu_open = false;
                }
                true
            }

            display::ACTION_MOVE => {
                let Some(from) = self.tab_at(&event.target) else { return false };
                let Ok(to) = event.value.parse::<usize>() else { return false };
                if to >= self.sheets.len() || to == from {
                    return false;
                }
                let looking_at = self.sheets[self.sheet].0;
                let moved = self.sheets.remove(from);
                self.note = format!("{} moved to {}", moved.1, to + 1);
                self.sheets.insert(to, moved);
                self.sheet =
                    self.sheets.iter().position(|(id, _)| *id == looking_at).unwrap_or(0);
                true
            }

            display::ACTION_CLICK => {
                self.menu_open = false;
                match event.target.as_str() {
                    "new-sheet" => {
                        let number = self.next_sheet;
                        self.next_sheet += 1;
                        self.sheets.push((number, format!("Sheet {number}")));
                        self.sheet = self.sheets.len() - 1;
                        self.cursor = (0, 0);
                        self.range = None;
                        self.out.restart();
                        self.publish_all(out);
                        self.note = "sheet added".to_owned();
                    }
                    "menu-clear" => {
                        for at in self.chosen() {
                            self.put(at, "");
                            self.publish(out, at);
                        }
                        self.note = "cleared".to_owned();
                    }
                    "menu-fill" => {
                        let chosen = self.chosen();
                        let Some(&first) = chosen.first() else { return true };
                        let value = self.get(first).to_owned();
                        for at in chosen {
                            self.put(at, &value);
                            self.publish(out, at);
                        }
                        self.note = "filled".to_owned();
                    }
                    other => {
                        log(&format!("nothing here answers to {other}"));
                        return false;
                    }
                }
                true
            }

            _ => false,
        }
    }

    /// The cells a command acts on: the run if there is one, else the cursor.
    fn chosen(&self) -> Vec<Ref> {
        let ((x0, y0), (x1, y1)) = match self.range {
            Some((from, to)) => (
                (from.0.min(to.0), from.1.min(to.1)),
                (from.0.max(to.0), from.1.max(to.1)),
            ),
            None => (self.cursor, self.cursor),
        };
        let mut out = Vec::new();
        for row in y0..=y1 {
            for column in x0..=x1 {
                out.push((column, row));
            }
        }
        out
    }

    /// Which sheet a tab's id names, if it names one.
    fn tab_at(&self, id: &str) -> Option<usize> {
        let number: u32 = id.strip_prefix("tab-")?.parse().ok()?;
        self.sheets.iter().position(|(sheet, _)| *sheet == number)
    }

    fn render(&self) -> String {
        let mut out = String::from("<window title=\"Sheet\" font=\"sans\">\n");

        // The menu bar: menus are children of the window and nowhere else.
        let _ = writeln!(
            out,
            "  <menu id=\"edit\" label=\"Edit\"{} description=\"Commands for the chosen cells\">
    <menuitem id=\"menu-fill\" label=\"Fill from the first\" description=\"Copies the first chosen cell into the rest\"/>
    <menuitem id=\"menu-clear\" label=\"Clear\" description=\"Empties every chosen cell\"/>
  </menu>",
            if self.menu_open { " open=\"true\"" } else { "" }
        );

        out.push_str("  <vstack gap=\"sm\" grow=\"true\">\n");
        out.push_str("    <tabs gap=\"sm\">\n");
        for (at, (id, name)) in self.sheets.iter().enumerate() {
            let _ = writeln!(
                out,
                "      <tab id=\"tab-{id}\" label=\"{name}\" closable=\"true\" movable=\"true\"{} \
                 description=\"Shows {name}\"/>",
                if at == self.sheet { " selected=\"true\"" } else { "" },
            );
        }
        out.push_str(
            "      <button id=\"new-sheet\" label=\"+\" description=\"Adds a sheet\"/>\n    </tabs>\n",
        );
        let _ = writeln!(
            out,
            "    <text role=\"caption\" color=\"muted\">{}</text>",
            escape(if self.note.is_empty() {
                "A thousand rows, and not one of them in this markup."
            } else {
                &self.note
            })
        );

        // The grid. Everything in it is a number: how far it runs, which
        // sheet holds the cells, which version of that sheet this tree was
        // drawn against, where the cursor is and what is chosen. The cells
        // themselves went up the socket before this did.
        let selection = match self.range {
            Some((from, to)) => format!("{}:{}", cell_name(from), cell_name(to)),
            None => cell_name(self.cursor),
        };
        let _ = writeln!(
            out,
            "    <spreadsheet id=\"sheet\" grow=\"true\" source=\"{source}\" version=\"{version}\" \
             rows=\"{ROWS}\" columns=\"{COLUMNS}\" cursor=\"{cursor}\" selection=\"{selection}\" \
             description=\"The grid of cells, {ROWS} rows deep\"/>",
            source = self.out.source(),
            version = self.out.version(),
            cursor = cell_name(self.cursor),
        );
        out.push_str("  </vstack>\n</window>\n");
        out
    }
}

/// `B7` from coordinates, and back. The same lettering the compositor uses,
/// because it is the lettering every spreadsheet uses.
fn cell_name(at: Ref) -> String {
    let (mut column, mut name) = (at.0, Vec::new());
    loop {
        name.push(b'A' + (column % 26) as u8);
        if column < 26 {
            break;
        }
        column = column / 26 - 1;
    }
    name.reverse();
    format!("{}{}", String::from_utf8(name).unwrap_or_default(), at.1 + 1)
}

fn parse_cell(at: &str) -> Option<Ref> {
    let letters = at.len() - at.trim_start_matches(|c: char| c.is_ascii_uppercase()).len();
    if letters == 0 {
        return None;
    }
    let (name, number) = at.split_at(letters);
    let mut column: u32 = 0;
    for letter in name.bytes() {
        column = column.checked_mul(26)?.checked_add((letter - b'A' + 1) as u32)?;
    }
    let row: u32 = number.parse().ok()?;
    (row > 0).then(|| (column - 1, row - 1))
}

/// Log to the kernel ring buffer.
fn log(message: &str) {
    let line = format!("<6>awsheet: {message}\n");
    if let Ok(mut kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = kmsg.write_all(line.as_bytes());
    } else {
        eprintln!("awsheet: {message}");
    }
}
