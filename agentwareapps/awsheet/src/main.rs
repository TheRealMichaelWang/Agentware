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

    let mut sheet =
        Sheet { first: 0, cells: HashMap::new(), selected: None, note: String::new() };
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
            self.cells.insert(format!("A{row}"), (*a).to_owned());
            self.cells.insert(format!("B{row}"), (*b).to_owned());
            self.cells.insert(format!("C{row}"), (*c).to_owned());
            self.cells.insert(format!("D{row}"), (*d).to_owned());
        }
        // Something far down the sheet, so that reading past the window is
        // worth doing and its result is recognisable.
        self.cells.insert("A500".to_owned(), "five hundred".to_owned());
        self.cells.insert("B500".to_owned(), "42".to_owned());
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
                if event.value.is_empty() {
                    self.cells.remove(&event.target);
                } else {
                    self.cells.insert(event.target.clone(), event.value.clone());
                }
                self.note = format!("{} is now {:?}", event.target, event.value);
            }

            // The cell cursor moved, whether the human clicked it, arrowed on
            // to it, or an agent chose it. An application that wanted to keep
            // the cursor in view would move its window here.
            display::ACTION_SELECT => {
                self.selected = Some(event.target.clone());
                self.note = format!("{} selected", event.target);
            }

            display::ACTION_SUBMIT => {
                self.note = format!("{} committed", event.target);
            }

            _ => return false,
        }
        true
    }

    fn window_end(&self) -> i32 {
        (self.first + WINDOW).min(ROWS)
    }

    fn render(&self) -> String {
        let mut out = String::from("<window title=\"Sheet\" font=\"sans\">\n");
        out.push_str("  <vstack gap=\"sm\" grow=\"true\">\n");
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
            let _ = writeln!(out, "      <column label=\"{name}\" chars=\"10\"/>");
        }
        for row in self.first..self.window_end() {
            let number = row + 1;
            let _ = writeln!(out, "      <row label=\"{number}\">");
            for name in COLUMNS {
                let id = format!("{name}{number}");
                let value = self.cells.get(&id).map(String::as_str).unwrap_or("");
                let chosen = if self.selected.as_deref() == Some(id.as_str()) {
                    " selected=\"true\""
                } else {
                    ""
                };
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
