//! Open, save and folder dialogs.
//!
//! An application that needs a file asks the human, and it asks the same way
//! every other application asks: a `dialog` in its own tree, holding a
//! folder's contents, a name to type for a save, and the two buttons. The
//! third kind asks for a folder rather than a file: where to copy or move
//! things to. The
//! compositor makes it modal, for the human and for the agent, and the agent
//! sees it in the application's view exactly as it sees the rest, so an agent
//! choosing a file is an agent clicking a folder, clicking a file, and clicking
//! Open. No new surface, no special case.
//!
//! ## Why a library rather than a service
//!
//! A picker process would need a channel back to the application that asked,
//! and applications have exactly one channel, to the compositor. Routing a
//! chosen path through the compositor, or handing an open descriptor across
//! it, is a real design, and it is the right one on the day applications run
//! in their own mount namespaces and must not read the filesystem themselves.
//! That day is not today: nothing is namespaced, an application reads what it
//! opens, and the smallest true thing is a model every application can embed
//! and one render they all share. When the portal is built it can keep this
//! markup, because what the human sees need not change for the mechanism
//! behind it to.
//!
//! ## How an application uses it
//!
//! Hold `Option<FileDialog>`. Open one with [`FileDialog::open`] or
//! [`FileDialog::save`], append [`FileDialog::render`] to the window's
//! children (last, so it is in front), and hand every event to
//! [`FileDialog::accept`] first: it answers [`Answer::Ignored`] for events that
//! are not its own, and the application goes on to handle those itself.
//! [`Answer::Chosen`] carries the path and means the dialog is done; so does
//! [`Answer::Cancelled`]. Drop it and re-render.
//!
//! What it does not do: it does not open, read, write or create anything. It
//! chooses a path. Whether the file exists, whether overwriting is fine, and
//! what to do with it are the application's decisions.

use std::path::{Path, PathBuf};

use awproto::display::{self, Event, escape};

/// Which question the dialog is asking.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Purpose {
    /// Pick an existing file.
    Open,
    /// Pick a folder and a name.
    Save,
    /// Pick a folder: the one being shown when the human confirms.
    Folder,
}

/// What an event came to.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Answer {
    /// Not the dialog's event. The application handles it.
    Ignored,
    /// The dialog changed and wants re-rendering.
    Changed,
    /// The human chose. The dialog is finished.
    Chosen(PathBuf),
    /// The human backed out. The dialog is finished.
    Cancelled,
}

/// One row of the listing.
struct Entry {
    name: String,
    is_dir: bool,
}

/// Longest listing shown. A folder with more entries than this shows the
/// first `MAX_ENTRIES` in name order and says so; a tree with ten thousand
/// items is a tree nobody reads and nothing should have to lay out.
const MAX_ENTRIES: usize = 400;

/// The ids the dialog owns. An application must not use these for its own
/// controls while a dialog is open.
const ID_UP: &str = "file-dialog-up";
const ID_NAME: &str = "file-dialog-name";
const ID_CONFIRM: &str = "file-dialog-confirm";
const ID_CANCEL: &str = "file-dialog-cancel";
const ID_ENTRY: &str = "file-dialog-entry-";

pub struct FileDialog {
    purpose: Purpose,
    dir: PathBuf,
    entries: Vec<Entry>,
    /// Whether the listing was cut at `MAX_ENTRIES`.
    truncated: bool,
    /// The file picked in the listing, for Open; for Save a pick fills `name`.
    selected: Option<usize>,
    /// The name being typed, for Save.
    name: String,
    /// Why the last attempt did not go through, shown until the next change.
    error: Option<String>,
}

impl FileDialog {
    /// Ask for an existing file, starting in `dir`.
    pub fn open(dir: impl Into<PathBuf>) -> FileDialog {
        let mut dialog = FileDialog {
            purpose: Purpose::Open,
            dir: PathBuf::new(),
            entries: Vec::new(),
            truncated: false,
            selected: None,
            name: String::new(),
            error: None,
        };
        dialog.enter(dir.into());
        dialog
    }

    /// Ask for a place and a name to save under, starting in `dir` with
    /// `suggested` already in the name box.
    pub fn save(dir: impl Into<PathBuf>, suggested: &str) -> FileDialog {
        let mut dialog = FileDialog::open(dir);
        dialog.purpose = Purpose::Save;
        dialog.name = suggested.to_owned();
        dialog
    }

    /// Ask for a folder, starting in `dir`. Only folders are listed; the
    /// answer is whichever one is being shown when Choose is pressed.
    pub fn folder(dir: impl Into<PathBuf>) -> FileDialog {
        let mut dialog = FileDialog::open(dir);
        dialog.purpose = Purpose::Folder;
        // Rebuilt without files, now that the purpose is known.
        let dir = dialog.dir.clone();
        dialog.enter(dir);
        dialog
    }

    pub fn purpose(&self) -> Purpose {
        self.purpose
    }

    /// The folder being shown.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Move the listing to a folder. One that cannot be read is shown empty
    /// with the reason, rather than refused: the human can still go up.
    fn enter(&mut self, dir: PathBuf) {
        self.dir = dir;
        self.selected = None;
        self.error = None;
        self.entries.clear();
        self.truncated = false;

        let read = match std::fs::read_dir(&self.dir) {
            Ok(read) => read,
            Err(err) => {
                self.error = Some(format!("cannot read this folder: {err}"));
                return;
            }
        };
        let mut entries: Vec<Entry> = read
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                // Dotfiles are configuration and clutter, not documents.
                if name.starts_with('.') {
                    return None;
                }
                let is_dir = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);
                // A folder chooser lists folders. Files there would be
                // things that cannot be chosen, and a list of those is noise.
                if self.purpose == Purpose::Folder && !is_dir {
                    return None;
                }
                Some(Entry { name, is_dir })
            })
            .collect();
        // Folders first, then files, each in name order: the shape every
        // file browser has, so nobody has to learn this one.
        entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
        if entries.len() > MAX_ENTRIES {
            entries.truncate(MAX_ENTRIES);
            self.truncated = true;
        }
        self.entries = entries;
    }

    /// Hand the dialog an event. It answers `Ignored` for any that is not
    /// its own, so an application can offer it every event first.
    pub fn accept(&mut self, event: &Event) -> Answer {
        let target = event.target.as_str();
        match (target, event.action.as_str()) {
            (ID_UP, display::ACTION_CLICK) => {
                if let Some(parent) = self.dir.parent().map(Path::to_path_buf) {
                    self.enter(parent);
                }
                Answer::Changed
            }

            (ID_CANCEL, display::ACTION_CLICK) => Answer::Cancelled,

            (ID_CONFIRM, display::ACTION_CLICK) | (ID_NAME, display::ACTION_SUBMIT) => self.confirm(),

            (ID_NAME, display::ACTION_TYPE_TEXT) => {
                self.name = event.value.clone();
                self.error = None;
                Answer::Changed
            }

            (_, display::ACTION_CLICK) if target.starts_with(ID_ENTRY) => {
                let Some(index) = target[ID_ENTRY.len()..].parse::<usize>().ok() else {
                    return Answer::Ignored;
                };
                let Some(entry) = self.entries.get(index) else { return Answer::Ignored };
                if entry.is_dir {
                    let next = self.dir.join(&entry.name);
                    self.enter(next);
                } else {
                    self.selected = Some(index);
                    self.error = None;
                    if self.purpose == Purpose::Save {
                        self.name = entry.name.clone();
                    }
                }
                Answer::Changed
            }

            _ => Answer::Ignored,
        }
    }

    /// The Open or Save button. Whether the answer is a path is checked here
    /// so the application receives one it can use, or nothing.
    fn confirm(&mut self) -> Answer {
        match self.purpose {
            Purpose::Folder => Answer::Chosen(self.dir.clone()),
            Purpose::Open => match self.selected.and_then(|index| self.entries.get(index)) {
                Some(entry) => Answer::Chosen(self.dir.join(&entry.name)),
                None => {
                    self.error = Some("choose a file first".to_owned());
                    Answer::Changed
                }
            },
            Purpose::Save => {
                let name = self.name.trim();
                if name.is_empty() {
                    self.error = Some("give the file a name".to_owned());
                    return Answer::Changed;
                }
                if name.contains('/') {
                    self.error = Some("a name cannot contain a slash".to_owned());
                    return Answer::Changed;
                }
                let path = self.dir.join(name);
                if path.is_dir() {
                    self.error = Some("that name is a folder here".to_owned());
                    return Answer::Changed;
                }
                Answer::Chosen(path)
            }
        }
    }

    /// The dialog as markup: one `dialog` element, to be appended to the
    /// window's children.
    pub fn render(&self) -> String {
        let (label, verb) = match self.purpose {
            Purpose::Open => ("Open a file", "Open"),
            Purpose::Save => ("Save a file", "Save"),
            Purpose::Folder => ("Choose a folder", "Choose this folder"),
        };
        let mut out = String::new();
        out.push_str(&format!("  <dialog label=\"{label}\">\n    <vstack gap=\"sm\" grow=\"true\">\n"));

        // Where we are, and the way up.
        let at_root = self.dir.parent().is_none();
        out.push_str(&format!(
            "      <hstack gap=\"sm\">\n        <button id=\"{ID_UP}\" label=\"Up\"{disabled} \
             description=\"Goes to the folder containing this one\"/>\n        \
             <text grow=\"true\" font=\"mono\" color=\"muted\">{dir}</text>\n      </hstack>\n",
            disabled = if at_root { " disabled=\"true\"" } else { "" },
            dir = escape(&self.dir.to_string_lossy()),
        ));

        // The listing. Folders are entered, files are chosen.
        out.push_str("      <scroll grow=\"true\">\n        <list>\n");
        if self.entries.is_empty() {
            out.push_str(if self.purpose == Purpose::Folder {
                "          <text color=\"muted\">No folders here.</text>\n"
            } else {
                "          <text color=\"muted\">This folder is empty.</text>\n"
            });
        }
        for (index, entry) in self.entries.iter().enumerate() {
            let selected = self.selected == Some(index) && !entry.is_dir;
            let (label, description) = if entry.is_dir {
                (format!("{}/", entry.name), format!("Enters the folder {}", entry.name))
            } else {
                (entry.name.clone(), format!("Selects the file {}", entry.name))
            };
            out.push_str(&format!(
                "          <item id=\"{ID_ENTRY}{index}\" label=\"{label}\"{selected} description=\"{description}\"/>\n",
                label = escape(&label),
                selected = if selected { " selected=\"true\"" } else { "" },
                description = escape(&description),
            ));
        }
        if self.truncated {
            out.push_str(&format!(
                "          <text role=\"caption\" color=\"muted\">Showing the first {MAX_ENTRIES} entries.</text>\n"
            ));
        }
        out.push_str("        </list>\n      </scroll>\n");

        // The name, for a save.
        if self.purpose == Purpose::Save {
            out.push_str(&format!(
                "      <field id=\"{ID_NAME}\" placeholder=\"File name\" value=\"{value}\" \
                 description=\"The name to save the file under, in the folder shown\"/>\n",
                value = escape(&self.name),
            ));
        }

        if let Some(error) = &self.error {
            out.push_str(&format!(
                "      <text role=\"caption\" color=\"danger\">{}</text>\n",
                escape(error)
            ));
        }

        // The decision. Open is offered only once there is a file to open;
        // Save is always offered, and says why if the name will not do.
        let can_confirm = match self.purpose {
            Purpose::Open => self.selected.is_some(),
            Purpose::Save | Purpose::Folder => true,
        };
        let confirm_description = match self.purpose {
            Purpose::Open => "Opens the selected file",
            Purpose::Save => "Saves under the name given, in the folder shown",
            Purpose::Folder => "Chooses the folder shown",
        };
        out.push_str(&format!(
            "      <hstack gap=\"sm\">\n        <text grow=\"true\"/>\n        \
             <button id=\"{ID_CANCEL}\" label=\"Cancel\" description=\"Closes this dialog without choosing anything\"/>\n        \
             <button id=\"{ID_CONFIRM}\" label=\"{verb}\" emphasis=\"primary\"{disabled} description=\"{confirm_description}\"/>\n      \
             </hstack>\n    </vstack>\n  </dialog>\n",
            disabled = if can_confirm { "" } else { " disabled=\"true\"" },
        ));
        out
    }
}
