//! A spreadsheet.
//!
//! One tab is one file. It reads and writes CSV, which is the only format it
//! knows and the only one it claims: there are no formulas and no arithmetic,
//! and a cell holds the text that was typed into it.
//!
//! The shape to notice is that **the cells are not in the tree**. Everything
//! else an application draws is described by resending the whole document and
//! letting the compositor diff it, which works because everything else is
//! small. A sheet is not, so it goes the other way: the cells are published as
//! runs on their own frames, the tree carries the sheet's *name* and the
//! *version* it was drawn against, and the compositor holds the sheet and
//! paints from it. Nothing is compared with anything. Typing into a cell says
//! `put("B7", &["4711"])` and that is the entire message.
//!
//! The version is what ties the two together, and `SheetOut` owns it so this
//! cannot get it wrong. Cells first, then the tree that claims them: one
//! connection, so they arrive in that order.
//!
//! A `source` names a stream of cells and the compositor holds one sheet per
//! source, so **each open file has a stream of its own**. Switching tabs is
//! then a different `source` in the next tree and nothing else: no cells on
//! the wire, nothing diffed, nothing thrown away.
//!
//! Everything the human does arrives as ordinary events, with the cell named
//! beside the action, because a cell is a coordinate rather than a node. What
//! is chosen and where the cursor is are this application's state, sent on the
//! element, exactly as a field's value is.
//!
//! Usage: awsheet

mod csv;

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};

use awkit::{Answer, Confirm, Decision, FileDialog};
use awproto::display::{self, Event, SheetOut, Surface, escape};

/// How far a sheet runs before anybody has changed it, and the least it runs
/// after a file is read into it. Both are numbers the element declares;
/// nothing is described until something is put in it, so a thousand rows here
/// is a pair of digits in the markup rather than a thousand of anything.
const ROWS: u32 = 1000;
const COLUMNS: u32 = 26;

/// Where the dialogs start looking, until one of them has been somewhere else.
const HOME: &str = "/home";

struct Book {
    /// What has been typed, by sheet number and cell. Absent is empty, so a
    /// thousand rows of nothing cost nothing here as well.
    cells: HashMap<(u32, Ref), String>,
    /// Which sheet is on screen. The tabs choose it; the cells are kept per
    /// sheet, so switching is not a way of losing work.
    sheet: usize,
    /// The sheets there are, in the order their tabs are in.
    sheets: Vec<Page>,
    next_sheet: u32,
    /// The cell the cursor is on, and the run chosen, if there is one. Both
    /// are this application's: they arrive as events and go back on the
    /// element, which is what lets the compositor draw them.
    cursor: Ref,
    range: Option<(Ref, Ref)>,
    /// Which menu is showing its items, by id. The compositor opens and closes
    /// them and says which, so this only has to remember the answer.
    menu_open: Option<String>,
    /// The question on screen, if there is one. At most one at a time: a
    /// dialog is modal, so a second could not be reached anyway.
    asking: Option<Asking>,
    /// Where the last file came from or went, so the next dialog opens there.
    dir: PathBuf,
    /// Whether the human has said to close the window and lose what is unsaved.
    quit: bool,
    /// The last thing that happened, so what the window did is visible.
    note: String,
}

/// The question being asked, and what its answer is for.
enum Asking {
    Open(FileDialog),
    /// Somewhere to save the sheet that was on screen when it was asked.
    /// Which sheet is remembered by number rather than by position, because
    /// tabs can be dragged into another order while a dialog is open.
    SaveAs(FileDialog, u32),
    /// Whether to close a sheet whose changes are not saved.
    Closing(Confirm, u32),
    /// Whether to close the window, and the whole book with it.
    Quitting(Confirm),
}

/// One sheet in the book: one file, open.
struct Page {
    /// The number that is only ever this sheet's, and never reused: closing a
    /// sheet and adding another gives the new one a number of its own, so it
    /// cannot inherit a stream of cells that used to mean something else.
    id: u32,
    /// The file this sheet is, or nothing until it has been saved somewhere.
    path: Option<PathBuf>,
    /// Whether anything has changed since it was opened or last saved. What
    /// puts the mark on the tab, and what makes closing ask first.
    dirty: bool,
    /// How far it runs, columns then rows. The shape goes on the element and
    /// nowhere else, so growing a sheet is rendering a bigger one and
    /// shrinking it is rendering a smaller one.
    extent: Ref,
    /// The stream this sheet's cells are published on, and the version it is
    /// up to. One per sheet, which is what makes switching tabs free.
    out: SheetOut,
}

impl Page {
    fn new(id: u32) -> Page {
        Page {
            id,
            path: None,
            dirty: false,
            extent: (COLUMNS, ROWS),
            out: SheetOut::new(&format!("book/{id}")),
        }
    }

    /// What to call it: the file's name, or a placeholder until it has one.
    fn name(&self) -> String {
        match &self.path {
            Some(path) => leaf(path),
            None => format!("Untitled {}", self.id),
        }
    }

    /// The name with the mark that says it is not saved.
    fn label(&self) -> String {
        match self.dirty {
            true => format!("{} *", self.name()),
            false => self.name(),
        }
    }

    /// Where it is, in words, for the status line.
    fn where_(&self) -> String {
        match &self.path {
            Some(path) => path.display().to_string(),
            None => "not saved yet".to_owned(),
        }
    }
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
        sheets: vec![Page::new(1)],
        next_sheet: 2,
        cursor: (0, 0),
        range: None,
        menu_open: None,
        asking: None,
        dir: PathBuf::from(HOME),
        quit: false,
        note: String::new(),
    };

    // Nothing to publish: one empty sheet, and an empty sheet is a stream the
    // compositor has never heard of, which is already nothing.
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
        // beginning. It asks by name, and the name is a sheet's own, so the
        // one that fell behind is the one republished: the others are held
        // there and are not disturbed.
        for source in surface.resend() {
            let Some(page) = book.sheets.iter().position(|page| page.out.source() == source)
            else {
                log(&format!("asked to resend {source}, which is not a sheet here"));
                continue;
            };
            log(&format!("republishing {source} from the beginning"));
            book.publish_all(&mut surface, page);
        }

        // The cross on the window, which asks rather than closes. Nothing
        // unsaved means nothing to say, so the process goes; otherwise the
        // question goes on screen and the human answers it there.
        if event.action == display::ACTION_CLOSE && event.target.is_empty() {
            if !book.quitting() {
                return;
            }
            if let Err(err) = surface.render(&book.render()) {
                log(&format!("could not send a tree: {err}"));
            }
            continue;
        }

        // The staleness check reads the surface and publishing writes to it,
        // so the one thing the surface is asked for is taken out first.
        let stale = surface.is_stale(&event);
        if !book.accept(stale, &event, &mut surface) {
            continue;
        }
        if book.quit {
            return;
        }
        if let Err(err) = surface.render(&book.render()) {
            log(&format!("could not send a tree: {err}"));
            return;
        }
    }
}

impl Book {
    /// The sheet on screen. There is always one: the last tab cannot be
    /// closed, so the index is never past the end.
    fn page(&self) -> &Page {
        &self.sheets[self.sheet.min(self.sheets.len() - 1)]
    }

    fn get(&self, at: Ref) -> &str {
        self.cells.get(&(self.page().id, at)).map_or("", String::as_str)
    }

    fn put(&mut self, at: Ref, value: &str) {
        let key = (self.page().id, at);
        if value.is_empty() {
            self.cells.remove(&key);
        } else {
            self.cells.insert(key, value.to_owned());
        }
    }

    /// Say that the sheet on screen has changed since it was last saved.
    fn touch(&mut self) {
        let at = self.sheet;
        self.sheets[at].dirty = true;
    }

    /// How far the sheet on screen runs, columns then rows.
    fn extent(&self) -> Ref {
        self.page().extent
    }

    /// The furthest cell with anything in it, for one sheet. `None` when it is
    /// empty, which is the difference between a file of nothing and no file.
    fn used(&self, id: u32) -> Option<Ref> {
        self.cells
            .iter()
            .filter(|((sheet, _), _)| *sheet == id)
            .fold(None, |bounds, ((_, (column, row)), _)| match bounds {
                None => Some((*column, *row)),
                Some((x, y)) => Some((x.max(*column), y.max(*row))),
            })
    }

    // --- files ---------------------------------------------------------

    /// Read a file into a sheet of its own, and show it.
    ///
    /// An untitled sheet with nothing in it is reused rather than left behind,
    /// because opening a file from a window that has not been used yet should
    /// not leave an empty tab sitting next to it.
    ///
    /// A file already open is shown rather than opened again. Two tabs on one
    /// file are two sets of edits with one place to put them, so whichever was
    /// saved second would quietly be the only one that happened.
    fn open_file(&mut self, path: &Path, out: &mut Surface) {
        if let Some(page) =
            self.sheets.iter().position(|sheet| sheet.path.as_deref() == Some(path))
        {
            self.sheet = page;
            self.cursor = (0, 0);
            self.range = None;
            self.note = format!("{} is already open", leaf(path));
            return;
        }

        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) => {
                self.note = format!("could not read {}: {err}", leaf(path));
                return;
            }
        };

        let blank = self.page().path.is_none()
            && !self.page().dirty
            && self.used(self.page().id).is_none();
        if !blank {
            let number = self.next_sheet;
            self.next_sheet += 1;
            self.sheets.push(Page::new(number));
            self.sheet = self.sheets.len() - 1;
        }

        let id = self.page().id;
        self.cells.retain(|(sheet, _), _| *sheet != id);
        let rows = csv::parse(&text);
        let mut extent = (0u32, rows.len() as u32);
        for (row, values) in rows.iter().enumerate() {
            extent.0 = extent.0.max(values.len() as u32);
            for (column, value) in values.iter().enumerate() {
                if !value.is_empty() {
                    self.cells.insert((id, (column as u32, row as u32)), value.clone());
                }
            }
        }

        let at = self.sheet;
        // Room to keep working: a file of three columns still opens as a
        // sheet, not as three columns.
        self.sheets[at].extent = (extent.0.max(COLUMNS), extent.1.max(ROWS));
        self.sheets[at].path = Some(path.to_owned());
        self.sheets[at].dirty = false;
        self.cursor = (0, 0);
        self.range = None;
        if let Some(dir) = path.parent() {
            self.dir = dir.to_owned();
        }
        self.publish_all(out, at);
        self.note = format!("opened {}", path.display());
    }

    /// Write a sheet to the file it came from, or ask where to put it.
    fn save(&mut self, page: usize) {
        match self.sheets.get(page).and_then(|page| page.path.clone()) {
            Some(path) => self.write_to(page, &path),
            None => self.save_as(page),
        }
    }

    fn save_as(&mut self, page: usize) {
        let Some(sheet) = self.sheets.get(page) else { return };
        let suggested = match &sheet.path {
            Some(path) => leaf(path),
            None => "sheet.csv".to_owned(),
        };
        self.asking = Some(Asking::SaveAs(
            FileDialog::save(&self.dir, &suggested).only(&["csv"]),
            sheet.id,
        ));
    }

    /// Write one sheet out as CSV: the rectangle from A1 to the furthest cell
    /// with anything in it, which is what every spreadsheet saves and is the
    /// only part of a thousand-row sheet worth writing down.
    fn write_to(&mut self, page: usize, path: &Path) {
        let Some(id) = self.sheets.get(page).map(|page| page.id) else { return };
        let rows = match self.used(id) {
            None => Vec::new(),
            Some((columns, rows)) => (0..=rows)
                .map(|row| {
                    (0..=columns)
                        .map(|column| {
                            self.cells
                                .get(&(id, (column, row)))
                                .cloned()
                                .unwrap_or_default()
                        })
                        .collect()
                })
                .collect(),
        };

        // Written whole, synced, and renamed into place, so a save that fails
        // halfway leaves the file that was there rather than half of the new
        // one.
        //
        // The sync is not a nicety and this is what it cost to learn: without
        // it the rename reaches the disk and the contents do not, so a machine
        // that loses power between the two comes back with the file present and
        // empty, which is worse than either outcome the rename was for. It read
        // exactly like a bug in the state volume, because the save said it had
        // worked and the next boot showed nothing.
        let scratch = path.with_extension("csv.part");
        let written = write_synced(&scratch, &csv::write(&rows))
            .and_then(|()| std::fs::rename(&scratch, path))
            .and_then(|()| match path.parent() {
                // The directory entry the rename made is itself a write, and
                // it needs the same treatment as the contents did.
                Some(dir) => std::fs::File::open(dir)?.sync_all(),
                None => Ok(()),
            });
        if let Err(err) = written {
            let _ = std::fs::remove_file(&scratch);
            self.note = format!("could not save {}: {err}", leaf(path));
            return;
        }

        self.sheets[page].path = Some(path.to_owned());
        self.sheets[page].dirty = false;
        if let Some(dir) = path.parent() {
            self.dir = dir.to_owned();
        }
        self.note = format!("saved {}", path.display());
    }

    /// Close a sheet, having been told it is all right to lose what is in it.
    fn close(&mut self, page: usize, out: &mut Surface) {
        if self.sheets.len() <= 1 {
            self.note = "the last sheet stays open".to_owned();
            return;
        }
        // A closed sheet's cells are given back on both sides: dropped here,
        // and emptied there by a restart carrying nothing, which is the only
        // way the compositor can be told a stream is finished. Its number is
        // not reused, so the next sheet added cannot pick up what this one was.
        let id = self.sheets[page].id;
        let name = self.sheets[page].name();
        if let Err(err) = self.sheets[page].out.restart(out) {
            log(&format!("could not empty {id}: {err}"));
        }
        self.cells.retain(|(sheet, _), _| *sheet != id);
        self.sheets.remove(page);
        self.sheet = self.sheet.min(self.sheets.len() - 1);
        self.cursor = (0, 0);
        self.range = None;
        self.note = format!("closed {name}");
    }

    // --- the grid ------------------------------------------------------

    /// Insert or delete a row or a column at the cursor, and say what
    /// happened.
    ///
    /// A structural edit is the one thing that moves cells rather than
    /// changing them, and there is no delta shape for "everything below here
    /// slides down": there could be, and it would be a second way of saying
    /// something the protocol can already say. So the sheet is published
    /// again from the beginning, which is what `base = 0` is for and is
    /// honest about the cost, since inserting a row genuinely does change
    /// where most of what is filled lives.
    ///
    /// The shape goes on the element and nowhere else. Growing it is free at
    /// both ends: nothing is allocated here, because the cells are a map of
    /// what has been typed rather than a rectangle, and nothing is allocated
    /// in the compositor, because a cell's place there is arithmetic.
    /// Shrinking is where both sides drop what falls outside, and they must
    /// agree about it, which is why the shift below empties the row it is
    /// about to lose rather than leaving it to be cut.
    fn structure(&mut self, across: bool, insert: bool) -> String {
        let (columns, rows) = self.extent();
        let at = if across { self.cursor.0 } else { self.cursor.1 };
        let end = if across { columns } else { rows };
        if !insert && end <= 1 {
            return "a sheet keeps at least one of each".to_owned();
        }

        let id = self.page().id;
        let mut moved: HashMap<(u32, Ref), String> = HashMap::new();
        for ((sheet, (column, row)), value) in self.cells.drain() {
            if sheet != id {
                moved.insert((sheet, (column, row)), value);
                continue;
            }
            let along = if across { column } else { row };
            let along = match (insert, along.cmp(&at)) {
                // Above or to the left of the cut: where it always was.
                (_, std::cmp::Ordering::Less) => Some(along),
                // The row being deleted goes with it.
                (false, std::cmp::Ordering::Equal) => None,
                (true, _) => Some(along + 1),
                (false, _) => Some(along - 1),
            };
            let Some(along) = along else { continue };
            let at = if across { (along, row) } else { (column, along) };
            moved.insert((sheet, at), value);
        }
        self.cells = moved;

        let shape = match (across, insert) {
            (true, true) => (columns.saturating_add(1), rows),
            (true, false) => (columns - 1, rows),
            (false, true) => (columns, rows.saturating_add(1)),
            (false, false) => (columns, rows - 1),
        };
        let at_sheet = self.sheet;
        self.sheets[at_sheet].extent = shape;
        // The cursor cannot be left outside the sheet it is in.
        self.cursor = (self.cursor.0.min(shape.0 - 1), self.cursor.1.min(shape.1 - 1));
        self.range = None;
        self.touch();

        format!(
            "{} {} {}, now {} by {}",
            if insert { "inserted" } else { "deleted" },
            if across { "column" } else { "row" },
            if across { column_name(at) } else { (at + 1).to_string() },
            shape.0,
            shape.1,
        )
    }

    /// Publish one sheet from the beginning: forget what was there, then one
    /// run per row that has anything in it. What a snapshot is, and what a
    /// `sheet-resend` gets.
    ///
    /// The restart is sent rather than armed, so a sheet with nothing in it
    /// publishes as exactly that. Arming a flag that the next run would carry
    /// meant an empty sheet sent no runs and therefore nothing at all, and
    /// the compositor kept the sheet it already had.
    fn publish_all(&mut self, surface: &mut Surface, page: usize) {
        let Some(id) = self.sheets.get(page).map(|page| page.id) else { return };
        if let Err(err) = self.sheets[page].out.restart(surface) {
            log(&format!("could not start book/{id} over: {err}"));
            return;
        }

        let mut rows: HashMap<u32, Vec<(u32, String)>> = HashMap::new();
        for ((sheet, (column, row)), value) in &self.cells {
            if *sheet == id {
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
            if let Err(err) = self.sheets[page].out.put(surface, &at, &borrowed) {
                log(&format!("could not publish {at}: {err}"));
                return;
            }
        }
    }

    /// Publish one cell of the sheet on screen.
    fn publish(&mut self, surface: &mut Surface, at: Ref) {
        let value = self.get(at).to_owned();
        let name = cell_name(at);
        let page = self.sheet;
        if let Err(err) = self.sheets[page].out.put(surface, &name, &[&value]) {
            log(&format!("could not publish {name}: {err}"));
        }
    }

    // --- events --------------------------------------------------------

    /// Act on an event, or say that nothing changed.
    fn accept(&mut self, stale: bool, event: &Event, out: &mut Surface) -> bool {
        if stale && event.action == display::ACTION_CLICK {
            return false;
        }

        // The dialog first. Each owns its ids and answers Ignored for the
        // rest, so nothing below has to know which events are its.
        if self.asking.is_some() && self.answer(event, out) {
            return true;
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
                    self.note = format!("{} on screen", self.sheets[sheet].name());
                    // Nothing is published. Each sheet has a stream of its
                    // own and the compositor is already holding all of them,
                    // so changing tabs is changing which one the element
                    // points at: a different `source` in the next tree, and
                    // no cells on the wire at all.
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
                self.touch();
                self.note = format!("{} is now {:?}", cell_name(at), event.value);
                true
            }

            display::ACTION_SUBMIT => at.is_some(),

            // The other button over the grid. The application decides what a
            // context menu is, and here it is the one holding the commands
            // that act on the chosen cells.
            display::ACTION_CONTEXT => {
                self.menu_open = Some("edit".to_owned());
                true
            }

            display::ACTION_OPEN => {
                self.menu_open = Some(event.target.clone());
                true
            }

            display::ACTION_CLOSE => {
                if let Some(sheet) = self.tab_at(&event.target) {
                    self.ask_to_close(sheet, out);
                } else {
                    self.menu_open = None;
                }
                true
            }

            display::ACTION_MOVE => {
                let Some(from) = self.tab_at(&event.target) else { return false };
                let Ok(to) = event.value.parse::<usize>() else { return false };
                if to >= self.sheets.len() || to == from {
                    return false;
                }
                let looking_at = self.sheets[self.sheet].id;
                let moved = self.sheets.remove(from);
                self.note = format!("{} moved to {}", moved.name(), to + 1);
                self.sheets.insert(to, moved);
                self.sheet =
                    self.sheets.iter().position(|page| page.id == looking_at).unwrap_or(0);
                true
            }

            display::ACTION_CLICK => {
                self.menu_open = None;
                match event.target.as_str() {
                    "new-sheet" | "menu-new" => {
                        let number = self.next_sheet;
                        self.next_sheet += 1;
                        self.sheets.push(Page::new(number));
                        self.sheet = self.sheets.len() - 1;
                        self.cursor = (0, 0);
                        self.range = None;
                        // Nothing to publish: a new sheet is empty, and its
                        // stream is one the compositor has never heard of, so
                        // there is already nothing there.
                        self.note = format!("{} added", self.page().name());
                    }
                    "menu-open" => {
                        self.asking =
                            Some(Asking::Open(FileDialog::open(&self.dir).only(&["csv"])));
                    }
                    "menu-save" => self.save(self.sheet),
                    "menu-save-as" => self.save_as(self.sheet),
                    "menu-clear" => {
                        for at in self.chosen() {
                            self.put(at, "");
                            self.publish(out, at);
                        }
                        self.touch();
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
                        self.touch();
                        self.note = "filled".to_owned();
                    }
                    // The four that change the sheet's shape rather than its
                    // contents. Cells first and then the tree that claims
                    // them, as always: the shifted sheet is published from
                    // the beginning, and the shape it is published against
                    // goes up on the element in the render that follows.
                    id @ ("menu-insert-row" | "menu-delete-row" | "menu-insert-column"
                    | "menu-delete-column") => {
                        let across = id.ends_with("column");
                        let insert = id.starts_with("menu-insert");
                        self.note = self.structure(across, insert);
                        self.publish_all(out, self.sheet);
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

    /// Closing a sheet with unsaved changes asks first. A saved one, or one
    /// that never had anything in it, goes without a question: a dialog that
    /// appears when nothing is at stake is one people learn to click through.
    fn ask_to_close(&mut self, page: usize, out: &mut Surface) {
        if !self.sheets[page].dirty {
            self.close(page, out);
            return;
        }
        if self.sheets.len() <= 1 {
            self.note = "the last sheet stays open".to_owned();
            return;
        }
        let sheet = &self.sheets[page];
        self.asking = Some(Asking::Closing(
            Confirm::new(
                "Unsaved changes",
                &format!("{} has changes that are not saved. Close it anyway?", sheet.name()),
                "Close without saving",
            )
            .danger(),
            sheet.id,
        ));
    }

    /// The human pressed the cross on the window. Says whether to carry on:
    /// false means there is nothing to lose and the process should end.
    ///
    /// The compositor asks rather than closing, and it takes silence for an
    /// answer: if this puts no question on screen, the next press closes the
    /// window regardless. So the only thing worth stopping for is unsaved work,
    /// and the question names it, since "one of your sheets" is not something a
    /// person can act on.
    fn quitting(&mut self) -> bool {
        // A question is already up. Leaving it there is the right answer:
        // whatever it is asking still has to be answered, and the compositor
        // sees a dialog either way.
        if self.asking.is_some() {
            return true;
        }

        let unsaved: Vec<String> =
            self.sheets.iter().filter(|page| page.dirty).map(Page::name).collect();
        if unsaved.is_empty() {
            return false;
        }
        let message = match unsaved.len() {
            1 => format!("{} has changes that are not saved.", unsaved[0]),
            _ => format!("{} have changes that are not saved.", unsaved.join(", ")),
        };
        self.asking = Some(Asking::Quitting(
            Confirm::new(
                "Unsaved changes",
                &format!("{message} Close the window anyway?"),
                "Close without saving",
            )
            .danger(),
        ));
        true
    }

    /// Hand an event to the open dialog, and act on what it answers. Says
    /// whether the dialog took it.
    fn answer(&mut self, event: &Event, out: &mut Surface) -> bool {
        let Some(asking) = &mut self.asking else { return false };
        match asking {
            Asking::Open(dialog) => match dialog.accept(event) {
                Answer::Ignored => false,
                Answer::Changed => true,
                Answer::Cancelled => {
                    self.asking = None;
                    self.note = "cancelled".to_owned();
                    true
                }
                Answer::Chosen(path) => {
                    self.asking = None;
                    self.open_file(&path, out);
                    true
                }
            },
            Asking::SaveAs(dialog, id) => {
                let id = *id;
                match dialog.accept(event) {
                    Answer::Ignored => false,
                    Answer::Changed => true,
                    Answer::Cancelled => {
                        self.asking = None;
                        self.note = "cancelled".to_owned();
                        true
                    }
                    Answer::Chosen(path) => {
                        self.asking = None;
                        // The one format this reads is the one it writes, so
                        // a name given without an extension gets the right
                        // one rather than a file nothing will open again.
                        let path = match path.extension() {
                            Some(_) => path,
                            None => path.with_extension("csv"),
                        };
                        if let Some(page) = self.page_of(id) {
                            self.write_to(page, &path);
                        }
                        true
                    }
                }
            }
            Asking::Closing(confirm, id) => {
                let id = *id;
                match confirm.accept(event) {
                    Decision::Ignored => false,
                    Decision::Cancelled => {
                        self.asking = None;
                        self.note = "kept open".to_owned();
                        true
                    }
                    Decision::Confirmed => {
                        self.asking = None;
                        if let Some(page) = self.page_of(id) {
                            self.close(page, out);
                        }
                        true
                    }
                }
            }
            Asking::Quitting(confirm) => match confirm.accept(event) {
                Decision::Ignored => false,
                Decision::Cancelled => {
                    self.asking = None;
                    self.note = "kept open".to_owned();
                    true
                }
                // Nothing is torn down here. The process ending is what closes
                // the connection, and the compositor takes the window with it.
                Decision::Confirmed => {
                    self.quit = true;
                    true
                }
            },
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

    /// Where a sheet is now, by its number. Positions move while a dialog is
    /// open, because tabs can be dragged; a number does not.
    fn page_of(&self, id: u32) -> Option<usize> {
        self.sheets.iter().position(|page| page.id == id)
    }

    /// Which sheet a tab's id names, if it names one.
    fn tab_at(&self, id: &str) -> Option<usize> {
        let number: u32 = id.strip_prefix("tab-")?.parse().ok()?;
        self.page_of(number)
    }

    fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "<window title=\"{} - Sheet\" font=\"sans\">",
            escape(&self.page().label())
        );

        // The menu bar: menus are children of the window and nowhere else.
        let open = |id: &str| match self.menu_open.as_deref() == Some(id) {
            true => " open=\"true\"",
            false => "",
        };
        let _ = writeln!(
            out,
            "  <menu id=\"file\" label=\"File\"{} description=\"Commands for the file in this tab\">
    <menuitem id=\"menu-new\" label=\"New\" description=\"Opens an empty sheet in a new tab\"/>
    <menuitem id=\"menu-open\" label=\"Open...\" description=\"Reads a CSV file into a tab of its own\"/>
    <menuitem id=\"menu-save\" label=\"Save\" description=\"Writes this sheet back to its file, asking where to put it if it has none\"/>
    <menuitem id=\"menu-save-as\" label=\"Save as...\" description=\"Writes this sheet to a file chosen now\"/>
  </menu>",
            open("file")
        );
        let _ = writeln!(
            out,
            "  <menu id=\"edit\" label=\"Edit\"{} description=\"Commands for the chosen cells\">
    <menuitem id=\"menu-fill\" label=\"Fill from the first\" description=\"Copies the first chosen cell into the rest\"/>
    <menuitem id=\"menu-clear\" label=\"Clear\" description=\"Empties every chosen cell\"/>
    <menuitem id=\"menu-insert-row\" label=\"Insert row\" description=\"Adds a row above the cursor, moving what is below it down\"/>
    <menuitem id=\"menu-delete-row\" label=\"Delete row\" description=\"Removes the cursor's row, moving what is below it up\"/>
    <menuitem id=\"menu-insert-column\" label=\"Insert column\" description=\"Adds a column left of the cursor, moving what is right of it across\"/>
    <menuitem id=\"menu-delete-column\" label=\"Delete column\" description=\"Removes the cursor's column, moving what is right of it back\"/>
  </menu>",
            open("edit")
        );

        out.push_str("  <vstack gap=\"sm\" grow=\"true\">\n");
        out.push_str("    <tabs gap=\"sm\">\n");
        for (at, page) in self.sheets.iter().enumerate() {
            let _ = writeln!(
                out,
                "      <tab id=\"tab-{id}\" label=\"{label}\" closable=\"true\" movable=\"true\"{} \
                 description=\"Shows {name}\"/>",
                if at == self.sheet { " selected=\"true\"" } else { "" },
                id = page.id,
                label = escape(&page.label()),
                name = escape(&page.name()),
            );
        }
        out.push_str(
            "      <button id=\"new-sheet\" label=\"+\" description=\"Opens an empty sheet in a new tab\"/>\n    </tabs>\n",
        );
        // With nothing to report, the line says where the file is, which is
        // the thing about an open file that is otherwise nowhere on screen.
        let said = match self.note.is_empty() {
            true => self.page().where_(),
            false => self.note.clone(),
        };
        let _ = writeln!(out, "    <text role=\"caption\" color=\"muted\">{}</text>", escape(&said));

        // The grid. Everything in it is a number: how far it runs, which
        // sheet holds the cells, which version of that sheet this tree was
        // drawn against, where the cursor is and what is chosen. The cells
        // themselves went up the socket before this did.
        let selection = match self.range {
            Some((from, to)) => format!("{}:{}", cell_name(from), cell_name(to)),
            None => cell_name(self.cursor),
        };
        let (columns, rows) = self.extent();
        let _ = writeln!(
            out,
            "    <spreadsheet id=\"sheet\" grow=\"true\" source=\"{source}\" version=\"{version}\" \
             rows=\"{rows}\" columns=\"{columns}\" cursor=\"{cursor}\" selection=\"{selection}\" \
             description=\"The cells of {name}, {rows} rows deep\"/>",
            source = self.page().out.source(),
            version = self.page().out.version(),
            cursor = cell_name(self.cursor),
            name = escape(&self.page().name()),
        );
        out.push_str("  </vstack>\n");

        // The question, if there is one. In the application's own tree, which
        // is what makes it one document to the compositor and one view to an
        // agent; the compositor floats it and makes it modal.
        if let Some(asking) = &self.asking {
            out.push_str(&match asking {
                Asking::Open(dialog) | Asking::SaveAs(dialog, _) => dialog.render(),
                Asking::Closing(confirm, _) | Asking::Quitting(confirm) => confirm.render(),
            });
        }

        out.push_str("</window>\n");
        out
    }
}

/// Write a file and do not return until the disk has it.
fn write_synced(path: &Path, text: &str) -> std::io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()
}

/// The last part of a path, which is what a file is called.
fn leaf(path: &Path) -> String {
    path.file_name().map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned())
}

/// `B7` from coordinates, and back. The same lettering the compositor uses,
/// because it is the lettering every spreadsheet uses.
fn cell_name(at: Ref) -> String {
    format!("{}{}", column_name(at.0), at.1 + 1)
}

/// A column's letters: base 26 with no zero, so 26 is AA rather than BA.
fn column_name(mut column: u32) -> String {
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
