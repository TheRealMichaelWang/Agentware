//! A grid, to try the table elements against.
//!
//! Not the spreadsheet application: there are no formulas, no files and no
//! arithmetic. It is the smallest thing that exercises what the compositor
//! now offers, so that the elements can be used before anything is built on
//! them, and it is the reference for how an application drives a table.
//!
//! The shape to notice is the window. A thousand rows exist here; the tree
//! this sends holds the two dozen on screen, and `first-row` says which two
//! dozen they are. When the human turns the wheel, drags the bar, or an agent
//! asks to read further down, the compositor sends one `scroll` event
//! carrying the row that should now be first, and this answers by describing
//! that window instead. Nothing else moves rows: an application that ignored
//! the event would simply show the same rows forever, which is a thing it is
//! allowed to do.
//!
//! Everything else is the ordinary contract every application keeps: a model,
//! a `render`, the whole tree every time, hand-written ids that never change.
//! Cell ids are the spreadsheet's own names, `B7`, because an id is what an
//! agent says out loud and those are the names the grid already has.
//!
//! Usage: awsheet

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write as IoWrite;

use awproto::display::{self, Event, Surface, escape};

/// How many rows the sheet has. Only a window of them is ever described.
const ROWS: i32 = 1000;

/// How many rows to describe at once. More than fits on screen is harmless
/// and less is a window with a hole in it, so this is simply generous.
const WINDOW: i32 = 40;

const COLUMNS: [&str; 8] = ["A", "B", "C", "D", "E", "F", "G", "H"];

struct Sheet {
    /// Which row of the sheet the window starts at. The compositor asks for
    /// this to change and this is the only thing that changes it.
    first: i32,
    /// What has been typed, by cell name. Absent is empty, so a thousand rows
    /// of nothing cost nothing.
    cells: HashMap<String, String>,
    /// Which sheet is on screen. The tabs choose it; the cells are kept per
    /// sheet, so switching is not a way of losing work.
    sheet: usize,
    /// The sheets there are, each with a number that is only ever its own.
    /// Closing one takes it out of here, adding one puts it back and moving
    /// one changes where it is in the list, which is all a tab strip is: a
    /// list, an order, and which of it you are looking at.
    ///
    /// The number rather than the position is what the tab's id is built
    /// from. A positional id survives neither a close nor a move: the tab a
    /// name refers to would change under the hand carrying it, which is one
    /// of those bugs that only appears once something can be dragged.
    sheets: Vec<(u32, String)>,
    /// The number the next new sheet takes.
    next_sheet: u32,
    /// The run of cells chosen, as its two corners. One gesture, one piece of
    /// state: a drag across the grid and an agent's select-range both arrive
    /// as the same event and land here.
    range: Option<(String, String)>,
    /// The column whose header was pressed, and the row likewise.
    column: Option<String>,
    row: Option<String>,
    /// Whether the menu is showing its items, and what a right-press was on
    /// when it opened one. The application owns `open`, as it owns every
    /// other piece of state in its tree; the compositor asks.
    menu_open: bool,
    /// The cell the cursor is on, by name. The compositor keeps a focus ring
    /// of its own and carries it across a re-render, but which cell is
    /// current is the application's to know: it is what a formula bar would
    /// show and what a copy would copy, and it arrives as a `select` event
    /// whether the human clicked it, arrowed on to it, or an agent chose it.
    selected: Option<String>,
    /// The last thing that happened, so the window's own behaviour is visible
    /// while playing with it.
    note: String,
}

fn main() {
    let mut surface = match Surface::inherited() {
        Ok(surface) => surface,
        Err(err) => {
            log(&format!("no interface connection: {err}"));
            std::process::exit(1);
        }
    };

    let mut sheet = Sheet {
        first: 0,
        cells: HashMap::new(),
        sheet: 0,
        sheets: vec![(1, "Sheet 1".into()), (2, "Sheet 2".into()), (3, "Sheet 3".into())],
        next_sheet: 4,
        range: None,
        column: None,
        row: None,
        menu_open: false,
        selected: None,
        note: String::new(),
    };
    sheet.seed();

    if let Err(err) = surface.render(&sheet.render()) {
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

        if !sheet.accept(&surface, &event) {
            continue;
        }
        if let Err(err) = surface.render(&sheet.render()) {
            log(&format!("could not send a tree: {err}"));
            return;
        }
    }
}

impl Sheet {
    /// A little data, so the grid is not a thousand empty rows.
    fn seed(&mut self) {
        let rows: [(&str, &str, &str, &str); 5] = [
            ("Region", "Q1", "Q2", "Q3"),
            ("North", "1240", "1310", "1288"),
            ("South", "980", "1105", "1170"),
            ("East", "1512", "1490", "1533"),
            ("West", "870", "920", "1004"),
        ];
        for (at, (a, b, c, d)) in rows.iter().enumerate() {
            let row = at + 1;
            self.cells.insert(format!("Sheet 1:A{row}"), (*a).to_owned());
            self.cells.insert(format!("Sheet 1:B{row}"), (*b).to_owned());
            self.cells.insert(format!("Sheet 1:C{row}"), (*c).to_owned());
            self.cells.insert(format!("Sheet 1:D{row}"), (*d).to_owned());
        }
        // Something far down the sheet, so that reading past the window is
        // worth doing and its result is recognisable.
        self.cells.insert("Sheet 1:A500".to_owned(), "five hundred".to_owned());
        self.cells.insert("Sheet 1:B500".to_owned(), "42".to_owned());
    }

    fn accept(&mut self, surface: &Surface, event: &Event) -> bool {
        // A click on a stale tree is a click on a control that may no longer
        // mean what it meant. Typed text is different: the payload is the
        // intention, so a late one is still true.
        if surface.is_stale(event) && event.action == display::ACTION_CLICK {
            return false;
        }

        match event.action.as_str() {
            // The window moved. This is the only place `first` changes.
            display::ACTION_SCROLL => {
                let asked: i32 = event.value.parse().unwrap_or(0);
                let first = asked.clamp(0, (ROWS - 1).max(0));
                if first == self.first {
                    return false;
                }
                self.first = first;
                self.note = format!("showing rows {} to {}", first + 1, self.window_end());
            }

            // Every keystroke, as it happens, exactly as a field's arrive.
            display::ACTION_TYPE_TEXT => {
                let key = self.keyed(&event.target);
                if event.value.is_empty() {
                    self.cells.remove(&key);
                } else {
                    self.cells.insert(key, event.value.clone());
                }
                self.note = format!("{} is now {:?}", event.target, event.value);
            }

            // The cell cursor moved, whether the human clicked it, arrowed on
            // to it, or an agent chose it. An application that wanted to keep
            // the cursor in view would move its window here.
            display::ACTION_SELECT => {
                let target = event.target.clone();
                // Which of them it is comes from the id, because this
                // application chose the ids and knows its own naming. The
                // compositor reports the same verb for all three, which is
                // right: pressing any of them means choosing it.
                if let Some(name) = target.strip_prefix("col-") {
                    self.column = Some(name.to_owned());
                    self.row = None;
                    self.range = None;
                    self.note = format!("column {name} selected");
                } else if let Some(number) = target.strip_prefix("row-") {
                    self.row = Some(number.to_owned());
                    self.column = None;
                    self.range = None;
                    self.note = format!("row {number} selected");
                } else if let Some(at) = self.tab_at(&target) {
                    self.sheet = at;
                    self.note = format!("{} on screen", self.sheets[at].1);
                } else {
                    self.selected = Some(target.clone());
                    self.range = None;
                    self.column = None;
                    self.row = None;
                    self.note = format!("{target} selected");
                }
            }

            // A run of cells: the target is one corner, the value the other.
            display::ACTION_SELECT_RANGE => {
                self.range = Some((event.target.clone(), event.value.clone()));
                self.column = None;
                self.row = None;
                self.note = format!("{} through {}", event.target, event.value);
            }

            // The other button. What it means is this application's to
            // decide, and what it decides is to offer the menu.
            display::ACTION_CONTEXT => {
                self.menu_open = true;
                self.note = if event.target.is_empty() {
                    "menu".to_owned()
                } else {
                    format!("menu on {}", event.target)
                };
            }

            display::ACTION_OPEN => self.menu_open = true,

            // Both the menu and a tab's cross arrive as `close`; which one it
            // was is in the id, which this application chose.
            display::ACTION_CLOSE => {
                if let Some(at) = self.tab_at(&event.target) {
                    // The last one stays: a strip with nothing in it is a
                    // window with nothing to look at.
                    if self.sheets.len() > 1 {
                        self.sheets.remove(at);
                        self.sheet = self.sheet.min(self.sheets.len() - 1);
                        self.note = "sheet closed".to_owned();
                    }
                } else {
                    self.menu_open = false;
                }
            }

            // A tab carried somewhere else. The order is this application's,
            // so the compositor says where the hand put it and nothing more;
            // moving it is done here, and the new tree is the answer.
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
                // Which sheet is on screen is a sheet, not a position, so it
                // is found again rather than arithmetic'd.
                self.sheet = self
                    .sheets
                    .iter()
                    .position(|(id, _)| *id == looking_at)
                    .unwrap_or(0);
            }

            display::ACTION_CLICK => {
                self.menu_open = false;
                match event.target.as_str() {
                    "new-sheet" => {
                        let number = self.next_sheet;
                        self.next_sheet += 1;
                        self.sheets.push((number, format!("Sheet {number}")));
                        self.sheet = self.sheets.len() - 1;
                        self.note = "sheet added".to_owned();
                    }
                    "menu-clear" => {
                        for id in self.chosen() {
                            let key = self.keyed(&id);
                            self.cells.remove(&key);
                        }
                        self.note = "cleared".to_owned();
                    }
                    "menu-fill" => {
                        let chosen = self.chosen();
                        let from = chosen
                            .first()
                            .and_then(|id| self.cells.get(&self.keyed(id)))
                            .cloned();
                        if let Some(value) = from {
                            for id in chosen {
                                let key = self.keyed(&id);
                                self.cells.insert(key, value.clone());
                            }
                        }
                        self.note = "filled".to_owned();
                    }
                    other => self.note = format!("{other} pressed"),
                }
            }

            display::ACTION_SUBMIT => {
                self.note = format!("{} committed", event.target);
            }

            _ => return false,
        }
        true
    }

    /// Every cell the human has chosen: a run, a whole column, a whole row,
    /// or the one under the cursor. Only what is described is touched, which
    /// is honest about a window: an application that reached beyond it would
    /// be acting on rows nobody is looking at.
    fn chosen(&self) -> Vec<String> {
        if let Some((from, to)) = &self.range
            && let (Some(start), Some(end)) = (cell_at(from), cell_at(to))
        {
            let (c0, c1) = (start.0.min(end.0), start.0.max(end.0));
            let (r0, r1) = (start.1.min(end.1), start.1.max(end.1));
            let mut out = Vec::new();
            for name in COLUMNS.iter().take(c1 + 1).skip(c0) {
                for row in r0..=r1 {
                    out.push(format!("{name}{row}"));
                }
            }
            return out;
        }
        if let Some(name) = &self.column {
            return (self.first..self.window_end())
                .map(|row| format!("{name}{}", row + 1))
                .collect();
        }
        if let Some(number) = &self.row {
            return COLUMNS.iter().map(|name| format!("{name}{number}")).collect();
        }
        self.selected.clone().into_iter().collect()
    }

    /// Whether a cell carries the highlight: it is in the chosen run, in the
    /// chosen column or row, or it is the one the cursor is on.
    fn is_chosen(&self, id: &str) -> bool {
        if self.range.is_some() || self.column.is_some() || self.row.is_some() {
            return self.chosen().iter().any(|chosen| chosen == id);
        }
        self.selected.as_deref() == Some(id)
    }

    fn window_end(&self) -> i32 {
        (self.first + WINDOW).min(ROWS)
    }

    /// A cell's name with its sheet in front of it, which is what the map is
    /// keyed by. The ids in the tree stay the spreadsheet's own names,
    /// because those are what an agent says out loud.
    fn keyed(&self, id: &str) -> String {
        let name = self.sheets.get(self.sheet).map_or("", |(_, name)| name.as_str());
        format!("{name}:{id}")
    }

    /// Which sheet a tab's id names, if it names one.
    fn tab_at(&self, id: &str) -> Option<usize> {
        let number: u32 = id.strip_prefix("tab-")?.parse().ok()?;
        self.sheets.iter().position(|(sheet, _)| *sheet == number)
    }

    fn render(&self) -> String {
        let mut out = String::from("<window title=\"Sheet\" font=\"sans\">\n");
        out.push_str("  <vstack gap=\"sm\" grow=\"true\">\n");

        // The tabs, then a menu bar. Both are ordinary controls in the tree;
        // the compositor floats the menu's items and knows nothing else about
        // either.
        // The same gap the navigation bar puts between its agentdesk tabs.
        // The strip holds the tabs, the plus that adds one, and the menu,
        // exactly as the navigation bar holds its tabs and its own plus. The
        // compositor paints the band; everything in it comes out looking
        // like the bar without this having to ask.
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
            "      <button id=\"new-sheet\" label=\"+\" description=\"Adds a sheet\"/>\n",
        );
        let _ = writeln!(
            out,
            "      <menu id=\"edit\" label=\"Edit\"{} description=\"Commands for the chosen cells\">
        <menuitem id=\"menu-fill\" label=\"Fill from the first\" description=\"Copies the first chosen cell into the rest\"/>
        <menuitem id=\"menu-clear\" label=\"Clear\" description=\"Empties every chosen cell\"/>
      </menu>",
            if self.menu_open { " open=\"true\"" } else { "" }
        );
        out.push_str("    </tabs>\n");
        let _ = writeln!(
            out,
            "    <text role=\"caption\" color=\"muted\">{}</text>",
            escape(if self.note.is_empty() {
                "A thousand rows. Only what fits is ever described."
            } else {
                &self.note
            })
        );

        // The table. `rows` is the sheet, the `row` children are the window,
        // and `first-row` is what ties the two together.
        let _ = writeln!(
            out,
            "    <table id=\"sheet\" grow=\"true\" rows=\"{ROWS}\" first-row=\"{}\" \
             description=\"The grid of cells, {ROWS} rows deep\">",
            self.first
        );
        for name in COLUMNS {
            let _ = writeln!(
                out,
                "      <column id=\"col-{name}\" label=\"{name}\" chars=\"10\"{} description=\"Column {name}\"/>",
                if self.column.as_deref() == Some(name) { " selected=\"true\"" } else { "" }
            );
        }
        for row in self.first..self.window_end() {
            let number = row + 1;
            let _ = writeln!(
                out,
                "      <row id=\"row-{number}\" label=\"{number}\"{} description=\"Row {number}\">",
                if self.row.as_deref() == Some(number.to_string().as_str()) {
                    " selected=\"true\""
                } else {
                    ""
                }
            );
            for name in COLUMNS {
                let id = format!("{name}{number}");
                let value = self.cells.get(&self.keyed(&id)).map(String::as_str).unwrap_or("");
                let chosen = if self.is_chosen(&id) { " selected=\"true\"" } else { "" };
                let _ = writeln!(
                    out,
                    "        <cell id=\"{id}\" value=\"{}\" editable=\"true\"{chosen}/>",
                    escape(value)
                );
            }
            out.push_str("      </row>\n");
        }
        out.push_str("    </table>\n  </vstack>\n</window>\n");
        out
    }
}

/// A cell's name back into a column index and a row number, so that a run
/// between two corners can be walked. `B7` is column one, row seven.
fn cell_at(id: &str) -> Option<(usize, i32)> {
    let letters: String = id.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    let digits: String = id.chars().skip_while(|c| c.is_ascii_alphabetic()).collect();
    let column = COLUMNS.iter().position(|name| *name == letters)?;
    Some((column, digits.parse().ok()?))
}

/// Log to the kernel ring buffer. An application has no console worth writing
/// to: the compositor owns the screen.
fn log(message: &str) {
    let line = format!("<6>awsheet: {message}\n");
    if let Ok(mut kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = kmsg.write_all(line.as_bytes());
    } else {
        eprintln!("awsheet: {message}");
    }
}
