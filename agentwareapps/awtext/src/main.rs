//! A text editor.
//!
//! One tab is one open file, the way one tab of `awsheet` is one open sheet.
//! It reads and writes plain text and nothing else: no syntax, no wrapping
//! rules of its own, no encodings beyond what a Rust `String` is.
//!
//! Where the spreadsheet is interesting, this is deliberately dull, and the
//! contrast is the point. A sheet's cells are the one thing in the system that
//! does not travel in the tree, because a screenful of them is four hundred
//! elements and a keystroke would re-serialise the lot. A document is one
//! string in one `editor`, so it goes the ordinary way: the whole tree every
//! time, the compositor diffs it, and a file of a few thousand characters costs
//! about what a dialog costs. That is the rule, and the sheet is the exception
//! that had to earn itself.
//!
//! The caret, the selection and the scroll position inside the box are the
//! compositor's and never appear here. What this owns is the text, which is
//! what a file is.
//!
//! Usage: awtext

use std::fmt::Write as _;
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};

use awkit::{Answer, Confirm, Decision, FileDialog};
use awproto::display::{self, Event, Surface, escape};

/// Where the dialogs start looking, until one of them has been somewhere else.
const HOME: &str = "/home";

/// The extensions the dialogs offer. Plain text is what this reads, and the
/// list is what a person is likely to have written it into.
const KINDS: &[&str] = &["txt", "md", "log", "conf", "xml", "json"];

/// The id of the one control that holds the document.
const BODY: &str = "body";

struct Editor {
    /// The files open, in the order their tabs are in.
    pages: Vec<Page>,
    /// Which one is on screen.
    page: usize,
    next: u32,
    /// The question on screen, if there is one. At most one at a time: a
    /// dialog is modal, so a second could not be reached anyway.
    asking: Option<Asking>,
    /// Whether the File menu is showing its items. The application owns this,
    /// as it owns every other piece of state: the compositor says the menu was
    /// asked to open and this answers by rendering it open.
    menu_open: bool,
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
    /// Somewhere to save a file. Which one is remembered by number rather than
    /// by position, because tabs can be dragged while a dialog is open.
    SaveAs(FileDialog, u32),
    /// Whether to close a file whose changes are not saved.
    Closing(Confirm, u32),
    /// Whether to close the window, and everything open in it.
    Quitting(Confirm),
}

/// One file, open.
struct Page {
    /// The number that is only ever this file's, and never reused. It names the
    /// tab, so it has to survive the tabs being dragged into another order.
    id: u32,
    /// The file this is, or nothing until it has been saved somewhere.
    path: Option<PathBuf>,
    /// What is in it. The whole document, in memory, which is what an editor
    /// this simple is.
    text: String,
    /// Whether it has changed since it was opened or last saved.
    dirty: bool,
}

impl Page {
    fn new(id: u32) -> Page {
        Page { id, path: None, text: String::new(), dirty: false }
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

fn main() {
    let mut surface = match Surface::inherited() {
        Ok(surface) => surface,
        Err(err) => {
            log(&format!("no interface connection: {err}"));
            std::process::exit(1);
        }
    };

    let mut editor = Editor {
        pages: vec![Page::new(1)],
        page: 0,
        next: 2,
        asking: None,
        menu_open: false,
        dir: PathBuf::from(HOME),
        quit: false,
        note: String::new(),
    };

    if let Err(err) = surface.render(&editor.render()) {
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

        // The cross on the window, which asks rather than closes. Nothing
        // unsaved means nothing to say, so the process goes; otherwise the
        // question goes on screen and the human answers it there.
        if event.action == display::ACTION_CLOSE && event.target.is_empty() {
            if !editor.quitting() {
                return;
            }
            if let Err(err) = surface.render(&editor.render()) {
                log(&format!("could not send a tree: {err}"));
            }
            continue;
        }

        if !editor.accept(surface.is_stale(&event), &event) {
            continue;
        }
        if editor.quit {
            return;
        }
        if let Err(err) = surface.render(&editor.render()) {
            log(&format!("could not send a tree: {err}"));
            return;
        }
    }
}

impl Editor {
    /// The file on screen. There is always one: the last tab cannot be closed,
    /// so the index is never past the end.
    fn page(&self) -> &Page {
        &self.pages[self.page.min(self.pages.len() - 1)]
    }

    /// Where a file is now, by its number. Positions move while a dialog is
    /// open, because tabs can be dragged; a number does not.
    fn page_of(&self, id: u32) -> Option<usize> {
        self.pages.iter().position(|page| page.id == id)
    }

    /// Which file a tab's id names, if it names one.
    fn tab_at(&self, id: &str) -> Option<usize> {
        let number: u32 = id.strip_prefix("tab-")?.parse().ok()?;
        self.page_of(number)
    }

    // --- files ---------------------------------------------------------

    /// Read a file into a tab of its own, and show it.
    ///
    /// An untitled tab with nothing in it is reused rather than left behind,
    /// and a file already open is shown rather than opened again: two tabs on
    /// one file are two sets of edits with one place to put them, so whichever
    /// was saved second would quietly be the only one that happened.
    fn open_file(&mut self, path: &Path) {
        if let Some(page) = self.pages.iter().position(|p| p.path.as_deref() == Some(path)) {
            self.page = page;
            self.note = format!("{} is already open", leaf(path));
            return;
        }

        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) => {
                // A file this cannot read is usually one that is not text, and
                // saying so is more use than the error the decoder gives.
                self.note = format!("could not read {}: {err}", leaf(path));
                return;
            }
        };

        let blank = self.page().path.is_none() && !self.page().dirty && self.page().text.is_empty();
        if !blank {
            let id = self.next;
            self.next += 1;
            self.pages.push(Page::new(id));
            self.page = self.pages.len() - 1;
        }

        let at = self.page;
        self.pages[at].text = text;
        self.pages[at].path = Some(path.to_owned());
        self.pages[at].dirty = false;
        if let Some(dir) = path.parent() {
            self.dir = dir.to_owned();
        }
        self.note = format!("opened {}", path.display());
    }

    /// Write a file to where it came from, or ask where to put it.
    fn save(&mut self, page: usize) {
        match self.pages.get(page).and_then(|page| page.path.clone()) {
            Some(path) => self.write_to(page, &path),
            None => self.save_as(page),
        }
    }

    fn save_as(&mut self, page: usize) {
        let Some(sheet) = self.pages.get(page) else { return };
        let suggested = match &sheet.path {
            Some(path) => leaf(path),
            None => "untitled.txt".to_owned(),
        };
        self.asking = Some(Asking::SaveAs(
            FileDialog::save(&self.dir, &suggested).only(KINDS),
            sheet.id,
        ));
    }

    /// Write one document out.
    ///
    /// Written whole, synced, and renamed into place. The sync is not a
    /// nicety: without it the rename reaches the disk and the contents do not,
    /// so a machine that loses power between the two comes back with the file
    /// present and empty, which is worse than either outcome the rename was
    /// for.
    fn write_to(&mut self, page: usize, path: &Path) {
        let Some(text) = self.pages.get(page).map(|page| page.text.clone()) else { return };
        let scratch = path.with_extension("part");
        let written = write_synced(&scratch, &text)
            .and_then(|()| std::fs::rename(&scratch, path))
            .and_then(|()| match path.parent() {
                // The directory entry the rename made is itself a write, and it
                // needs the same treatment the contents did.
                Some(dir) => std::fs::File::open(dir)?.sync_all(),
                None => Ok(()),
            });
        if let Err(err) = written {
            let _ = std::fs::remove_file(&scratch);
            self.note = format!("could not save {}: {err}", leaf(path));
            return;
        }

        self.pages[page].path = Some(path.to_owned());
        self.pages[page].dirty = false;
        if let Some(dir) = path.parent() {
            self.dir = dir.to_owned();
        }
        self.note = format!("saved {}", path.display());
    }

    /// Close a tab, having been told it is all right to lose what is in it.
    fn close(&mut self, page: usize) {
        if self.pages.len() <= 1 {
            self.note = "the last tab stays open".to_owned();
            return;
        }
        let name = self.pages[page].name();
        self.pages.remove(page);
        self.page = self.page.min(self.pages.len() - 1);
        self.note = format!("closed {name}");
    }

    /// Closing a tab with unsaved changes asks first. A saved one goes without
    /// a question: a dialog that appears when nothing is at stake is one people
    /// learn to click through.
    fn ask_to_close(&mut self, page: usize) {
        if !self.pages[page].dirty {
            self.close(page);
            return;
        }
        if self.pages.len() <= 1 {
            self.note = "the last tab stays open".to_owned();
            return;
        }
        let page = &self.pages[page];
        self.asking = Some(Asking::Closing(
            Confirm::new(
                "Unsaved changes",
                &format!("{} has changes that are not saved. Close it anyway?", page.name()),
                "Close without saving",
            )
            .danger(),
            page.id,
        ));
    }

    /// The human pressed the cross on the window. Says whether to carry on:
    /// false means there is nothing to lose and the process should end.
    fn quitting(&mut self) -> bool {
        // A question is already up. Leaving it there is the right answer:
        // whatever it asks still has to be answered.
        if self.asking.is_some() {
            return true;
        }

        let unsaved: Vec<String> =
            self.pages.iter().filter(|page| page.dirty).map(Page::name).collect();
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

    // --- events --------------------------------------------------------

    /// Act on an event, or say that nothing changed.
    fn accept(&mut self, stale: bool, event: &Event) -> bool {
        if stale && event.action == display::ACTION_CLICK {
            return false;
        }

        // The dialog first. Each owns its ids and answers Ignored for the rest,
        // so nothing below has to know which events are its.
        if self.asking.is_some() && self.answer(event) {
            return true;
        }

        match event.action.as_str() {
            // A keystroke in the box, carrying the whole document as it now
            // stands. One event per keystroke, the same for a person's typing
            // and an agent's, so this cannot tell them apart. An agent's
            // `clear` arrives here too, as the empty value it amounts to, for
            // the same reason: there is one path in and no second one to drift
            // from it.
            display::ACTION_TYPE_TEXT if event.target == BODY => {
                let at = self.page;
                if self.pages[at].text == event.value {
                    return false;
                }
                self.pages[at].text = event.value.clone();
                self.pages[at].dirty = true;
                self.note.clear();
                true
            }

            display::ACTION_SELECT => match self.tab_at(&event.target) {
                Some(page) => {
                    self.page = page;
                    self.note = format!("{} on screen", self.pages[page].name());
                    true
                }
                None => false,
            },

            display::ACTION_OPEN => {
                self.menu_open = true;
                true
            }

            display::ACTION_CLOSE => {
                match self.tab_at(&event.target) {
                    Some(page) => self.ask_to_close(page),
                    None => self.menu_open = false,
                }
                true
            }

            display::ACTION_MOVE => {
                let Some(from) = self.tab_at(&event.target) else { return false };
                let Ok(to) = event.value.parse::<usize>() else { return false };
                if to >= self.pages.len() || to == from {
                    return false;
                }
                let looking_at = self.pages[self.page].id;
                let moved = self.pages.remove(from);
                self.note = format!("{} moved to {}", moved.name(), to + 1);
                self.pages.insert(to, moved);
                self.page = self.pages.iter().position(|page| page.id == looking_at).unwrap_or(0);
                true
            }

            display::ACTION_CLICK => {
                self.menu_open = false;
                match event.target.as_str() {
                    "new-file" | "menu-new" => {
                        let id = self.next;
                        self.next += 1;
                        self.pages.push(Page::new(id));
                        self.page = self.pages.len() - 1;
                        self.note = format!("{} added", self.page().name());
                    }
                    "menu-open" => {
                        self.asking = Some(Asking::Open(FileDialog::open(&self.dir).only(KINDS)));
                    }
                    "menu-save" => self.save(self.page),
                    "menu-save-as" => self.save_as(self.page),
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

    /// Hand an event to the open dialog, and act on what it answers. Says
    /// whether the dialog took it.
    fn answer(&mut self, event: &Event) -> bool {
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
                    self.open_file(&path);
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
                        // A name given without an extension gets the ordinary
                        // one, rather than a file whose kind nothing can guess.
                        let path = match path.extension() {
                            Some(_) => path,
                            None => path.with_extension("txt"),
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
                            self.close(page);
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

    fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(
            out,
            "<window title=\"{} - Text\" font=\"sans\">",
            escape(&self.page().label())
        );

        // The menu bar: menus are children of the window and nowhere else.
        let _ = writeln!(
            out,
            "  <menu id=\"file\" label=\"File\"{} description=\"Commands for the file in this tab\">
    <menuitem id=\"menu-new\" label=\"New\" description=\"Opens an empty document in a new tab\"/>
    <menuitem id=\"menu-open\" label=\"Open...\" description=\"Reads a text file into a tab of its own\"/>
    <menuitem id=\"menu-save\" label=\"Save\" description=\"Writes this document back to its file, asking where to put it if it has none\"/>
    <menuitem id=\"menu-save-as\" label=\"Save as...\" description=\"Writes this document to a file chosen now\"/>
  </menu>",
            if self.menu_open { " open=\"true\"" } else { "" }
        );

        out.push_str("  <vstack gap=\"sm\" grow=\"true\">\n");
        out.push_str("    <tabs gap=\"sm\">\n");
        for (at, page) in self.pages.iter().enumerate() {
            let _ = writeln!(
                out,
                "      <tab id=\"tab-{id}\" label=\"{label}\" closable=\"true\" movable=\"true\"{} \
                 description=\"Shows {name}\"/>",
                if at == self.page { " selected=\"true\"" } else { "" },
                id = page.id,
                label = escape(&page.label()),
                name = escape(&page.name()),
            );
        }
        out.push_str(
            "      <button id=\"new-file\" label=\"+\" description=\"Opens an empty document in a new tab\"/>\n    </tabs>\n",
        );

        // With nothing to report, the line says where the file is, which is the
        // one thing about an open file that is otherwise nowhere on screen.
        let said = match self.note.is_empty() {
            true => self.page().where_(),
            false => self.note.clone(),
        };
        let _ = writeln!(out, "    <text role=\"caption\" color=\"muted\">{}</text>", escape(&said));

        // The document, whole, in the tree. Unlike a sheet there is no reason
        // for it to be anywhere else: it is one string, and the compositor
        // diffing it costs a comparison.
        let _ = writeln!(
            out,
            "    <editor id=\"{BODY}\" grow=\"true\" value=\"{value}\" \
             placeholder=\"Nothing here yet.\" description=\"The text of {name}\"/>",
            value = escape(&self.page().text),
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
    path.file_name()
        .map_or_else(|| path.display().to_string(), |name| name.to_string_lossy().into_owned())
}

/// Log to the kernel ring buffer.
fn log(message: &str) {
    let line = format!("<6>awtext: {message}\n");
    if let Ok(mut kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = kmsg.write_all(line.as_bytes());
    } else {
        eprintln!("awtext: {message}");
    }
}
