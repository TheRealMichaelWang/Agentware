//! Files: a file explorer, and the place the shared dialogs are seen at work.
//!
//! One folder at a time. Every row is a checkbox and a name: the checkbox marks
//! the file or folder for whatever comes next, the name enters a folder or
//! marks a file. The toolbar acts on what is marked: New folder, Rename (one
//! thing), Copy to..., Move to..., Delete. Copy and Move ask for a destination
//! with `awkit`'s folder dialog, Delete asks first with its confirm dialog,
//! and New folder and Rename ask for a name with its text prompt, so every
//! question this application asks is asked the way every application asks it:
//! a `dialog` in its own tree, modal by the compositor, and to the agent a few
//! controls nested in `<dialog>`.
//!
//! Marking is a checkbox rather than a modifier key because there are no
//! modifier keys in the event vocabulary and should not be: an agent marks a
//! file by naming its checkbox, exactly as a human does by pressing it, and
//! neither has to hold anything down.
//!
//! Everything it walks lives on the root filesystem, which is a disk: what
//! is made or moved here survives a reboot the way files on a computer do,
//! and is reset only when the OS image is rebuilt. It opens in `/home`,
//! which ships with a few files so there is something to find.
//!
//! Written the way every application is written: a model and a `render`, whole
//! tree every time, hand-written stable ids, no diffing and no ephemeral state.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};

use awkit::{Answer, Confirm, Decision, FileDialog, Reply, TextPrompt};
use awproto::display::{self, Event, Surface, escape};

/// Where the explorer opens.
const HOME: &str = "/home";

/// Longest listing shown, for the same reason the dialog has one.
const MAX_ENTRIES: usize = 400;

struct Entry {
    name: String,
    is_dir: bool,
    size: u64,
}

/// The one dialog open, if any, and what it is for.
enum Asking {
    /// A destination for a copy of what is marked.
    CopyTo(FileDialog),
    /// A destination for what is marked.
    MoveTo(FileDialog),
    /// Whether to delete what is marked.
    Delete(Confirm),
    /// A name for a new folder.
    NewFolder(TextPrompt),
    /// A new name for the one thing marked.
    Rename(TextPrompt),
}

struct Files {
    dir: PathBuf,
    entries: Vec<Entry>,
    truncated: bool,
    /// The names marked in the folder shown. Cleared on entering another.
    marked: BTreeSet<String>,
    status: String,
    asking: Option<Asking>,
}

fn main() {
    let mut surface = match Surface::inherited() {
        Ok(surface) => surface,
        Err(err) => {
            log(&format!("no interface connection: {err}"));
            std::process::exit(1);
        }
    };

    let mut app = Files::new(PathBuf::from(HOME));

    if let Err(err) = surface.render(&app.render()) {
        log(&format!("could not send the first tree: {err}"));
        std::process::exit(1);
    }

    loop {
        let event = match surface.next_event() {
            Ok(Some(event)) => event,
            Ok(None) => return,
            Err(err) => {
                log(&format!("connection failed: {err}"));
                std::process::exit(1);
            }
        };

        if !app.accept(&surface, &event) {
            continue;
        }

        if let Err(err) = surface.render(&app.render()) {
            log(&format!("could not send a tree: {err}"));
            return;
        }
    }
}

impl Files {
    fn new(dir: PathBuf) -> Files {
        let mut files = Files {
            dir: PathBuf::new(),
            entries: Vec::new(),
            truncated: false,
            marked: BTreeSet::new(),
            status: String::new(),
            asking: None,
        };
        files.enter(dir);
        files
    }

    /// Show a folder. Marks are dropped: they named things in the last one.
    fn enter(&mut self, dir: PathBuf) {
        self.dir = dir;
        self.marked.clear();
        self.reload();
        self.summarize();
    }

    /// Re-read the folder shown, keeping marks on names that still exist.
    fn reload(&mut self) {
        self.entries.clear();
        self.truncated = false;

        let read = match std::fs::read_dir(&self.dir) {
            Ok(read) => read,
            Err(err) => {
                self.status = format!("cannot read {}: {err}", self.dir.display());
                return;
            }
        };
        let mut entries: Vec<Entry> = read
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().into_string().ok()?;
                if name.starts_with('.') {
                    return None;
                }
                let meta = entry.metadata().ok()?;
                Some(Entry { name, is_dir: meta.is_dir(), size: meta.len() })
            })
            .collect();
        entries.sort_by(|a, b| b.is_dir.cmp(&a.is_dir).then_with(|| a.name.cmp(&b.name)));
        if entries.len() > MAX_ENTRIES {
            entries.truncate(MAX_ENTRIES);
            self.truncated = true;
        }
        self.entries = entries;
        let names: BTreeSet<&str> = self.entries.iter().map(|e| e.name.as_str()).collect();
        self.marked.retain(|name| names.contains(name.as_str()));
    }

    /// The status line when nothing more specific has just happened.
    fn summarize(&mut self) {
        self.status = format!(
            "{} item(s){}",
            self.entries.len(),
            if self.marked.is_empty() { String::new() } else { format!(", {} marked", self.marked.len()) }
        );
    }

    /// The marked things, as paths, in name order.
    fn marked_paths(&self) -> Vec<PathBuf> {
        self.marked.iter().map(|name| self.dir.join(name)).collect()
    }

    fn accept(&mut self, surface: &Surface, event: &Event) -> bool {
        if surface.is_stale(event) && event.action == display::ACTION_CLICK {
            self.status = format!("discarded a click on {} against an older tree", event.target);
            log(&self.status);
            return true;
        }

        // The dialog first. Each owns its ids and answers Ignored for the
        // rest, so nothing below has to know which events are its.
        if let Some(asking) = &mut self.asking {
            let done = match asking {
                Asking::CopyTo(dialog) | Asking::MoveTo(dialog) => match dialog.accept(event) {
                    Answer::Ignored => None,
                    Answer::Changed => return true,
                    Answer::Cancelled => Some(Outcome::Cancelled),
                    Answer::Chosen(path) => Some(Outcome::Path(path)),
                },
                Asking::Delete(confirm) => match confirm.accept(event) {
                    Decision::Ignored => None,
                    Decision::Cancelled => Some(Outcome::Cancelled),
                    Decision::Confirmed => Some(Outcome::Yes),
                },
                Asking::NewFolder(prompt) | Asking::Rename(prompt) => match prompt.accept(event) {
                    Reply::Ignored => None,
                    Reply::Changed => return true,
                    Reply::Cancelled => Some(Outcome::Cancelled),
                    Reply::Done(text) => Some(Outcome::Text(text)),
                },
            };
            if let Some(outcome) = done {
                self.settle(outcome);
                return true;
            }
        }

        match (event.target.as_str(), event.action.as_str()) {
            ("up", display::ACTION_CLICK) => {
                if let Some(parent) = self.dir.parent().map(Path::to_path_buf) {
                    self.enter(parent);
                }
            }

            ("mark-all", display::ACTION_CLICK) => {
                if self.marked.len() == self.entries.len() {
                    self.marked.clear();
                } else {
                    self.marked = self.entries.iter().map(|e| e.name.clone()).collect();
                }
                self.summarize();
            }

            ("new-folder", display::ACTION_CLICK) => {
                self.asking = Some(Asking::NewFolder(TextPrompt::new(
                    "New folder",
                    "Name for the new folder",
                    "",
                    "Create",
                )));
            }

            ("rename", display::ACTION_CLICK) => {
                let Some(name) = self.marked.iter().next().cloned() else { return true };
                if self.marked.len() != 1 {
                    self.status = "mark exactly one thing to rename it".into();
                    return true;
                }
                self.asking = Some(Asking::Rename(TextPrompt::new(
                    &format!("Rename {name}"),
                    "New name",
                    &name,
                    "Rename",
                )));
            }

            ("copy-to", display::ACTION_CLICK) => {
                if self.marked.is_empty() {
                    self.status = "mark something to copy first".into();
                    return true;
                }
                self.asking = Some(Asking::CopyTo(FileDialog::folder(&self.dir)));
            }

            ("move-to", display::ACTION_CLICK) => {
                if self.marked.is_empty() {
                    self.status = "mark something to move first".into();
                    return true;
                }
                self.asking = Some(Asking::MoveTo(FileDialog::folder(&self.dir)));
            }

            ("delete", display::ACTION_CLICK) => {
                if self.marked.is_empty() {
                    self.status = "mark something to delete first".into();
                    return true;
                }
                let what = describe(&self.marked);
                self.asking = Some(Asking::Delete(
                    Confirm::new(
                        "Delete",
                        &format!("Delete {what}? Folders go with everything in them. This cannot be undone."),
                        "Delete",
                    )
                    .danger(),
                ));
            }

            // A row's checkbox: mark or unmark.
            (target, display::ACTION_TOGGLE) if target.starts_with("mark-") => {
                let Some(entry) = target["mark-".len()..].parse::<usize>().ok().and_then(|i| self.entries.get(i))
                else {
                    return false;
                };
                let name = entry.name.clone();
                if !self.marked.remove(&name) {
                    self.marked.insert(name);
                }
                self.summarize();
            }

            // A row's name: enter a folder, or mark a file.
            (target, display::ACTION_CLICK) if target.starts_with("entry-") => {
                let Some(entry) = target["entry-".len()..].parse::<usize>().ok().and_then(|i| self.entries.get(i))
                else {
                    return false;
                };
                if entry.is_dir {
                    let next = self.dir.join(&entry.name);
                    self.enter(next);
                } else {
                    let name = entry.name.clone();
                    if !self.marked.remove(&name) {
                        self.marked.insert(name);
                    }
                    self.summarize();
                }
            }

            _ => return false,
        }
        true
    }

    /// A dialog answered. Do what was asked, or nothing if it was cancelled.
    fn settle(&mut self, outcome: Outcome) {
        let Some(asking) = self.asking.take() else { return };
        match (asking, outcome) {
            (_, Outcome::Cancelled) => self.status = "cancelled".into(),

            (Asking::CopyTo(_), Outcome::Path(to)) => {
                let (done, failed) = self.each_marked(|from| copy_recursive(from, &to.join(leaf(from))));
                self.status = format!("copied {done} item(s) to {}{failed}", to.display());
                self.reload();
            }

            (Asking::MoveTo(_), Outcome::Path(to)) => {
                let (done, failed) = self.each_marked(|from| move_path(from, &to.join(leaf(from))));
                self.status = format!("moved {done} item(s) to {}{failed}", to.display());
                self.reload();
            }

            (Asking::Delete(_), Outcome::Yes) => {
                let (done, failed) = self.each_marked(|path| {
                    if path.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) }
                });
                self.status = format!("deleted {done} item(s){failed}");
                self.reload();
            }

            (Asking::NewFolder(mut prompt), Outcome::Text(name)) => {
                if let Some(why) = bad_name(&name) {
                    prompt.refuse(why);
                    self.asking = Some(Asking::NewFolder(prompt));
                    return;
                }
                match std::fs::create_dir(self.dir.join(&name)) {
                    Ok(()) => {
                        self.status = format!("made {name}");
                        self.reload();
                    }
                    Err(err) => {
                        prompt.refuse(&format!("could not make it: {err}"));
                        self.asking = Some(Asking::NewFolder(prompt));
                    }
                }
            }

            (Asking::Rename(mut prompt), Outcome::Text(name)) => {
                if let Some(why) = bad_name(&name) {
                    prompt.refuse(why);
                    self.asking = Some(Asking::Rename(prompt));
                    return;
                }
                let Some(old) = self.marked.iter().next().cloned() else { return };
                match std::fs::rename(self.dir.join(&old), self.dir.join(&name)) {
                    Ok(()) => {
                        self.status = format!("renamed {old} to {name}");
                        self.marked.clear();
                        self.marked.insert(name);
                        self.reload();
                    }
                    Err(err) => {
                        prompt.refuse(&format!("could not rename it: {err}"));
                        self.asking = Some(Asking::Rename(prompt));
                    }
                }
            }

            // A dialog answered with the wrong kind of outcome cannot happen;
            // treated as cancelled rather than guessed at.
            _ => self.status = "cancelled".into(),
        }
        log(&self.status);
    }

    /// Run an operation over everything marked. Returns how many succeeded
    /// and, if any failed, a suffix saying so with the first reason.
    fn each_marked(&self, op: impl Fn(&Path) -> std::io::Result<()>) -> (usize, String) {
        let mut done = 0;
        let mut first_error: Option<String> = None;
        let mut failed = 0;
        for path in self.marked_paths() {
            match op(&path) {
                Ok(()) => done += 1,
                Err(err) => {
                    failed += 1;
                    first_error.get_or_insert_with(|| format!("{}: {err}", leaf(&path)));
                }
            }
        }
        let suffix = match first_error {
            Some(why) => format!("; {failed} failed ({why})"),
            None => String::new(),
        };
        (done, suffix)
    }

    fn render(&self) -> String {
        let mut out = String::new();
        let at_root = self.dir.parent().is_none();
        let marked = self.marked.len();
        let off = |allowed: bool| if allowed { "" } else { r#" disabled="true""# };
        let _ = write!(
            out,
            r#"<window title="Files" font="sans">
  <vstack gap="sm" grow="true">
    <hstack gap="sm">
      <button id="up" label="Up"{up} description="Goes to the folder containing this one"/>
      <text grow="true" font="mono" color="muted">{dir}</text>
      <button id="new-folder" label="New folder" description="Makes a folder here, after asking for its name"/>
      <button id="rename" label="Rename"{one} description="Renames the one marked file or folder, after asking for the new name"/>
      <button id="copy-to" label="Copy to..."{any} description="Copies the marked files and folders to a folder you choose"/>
      <button id="move-to" label="Move to..."{any} description="Moves the marked files and folders to a folder you choose"/>
      <button id="delete" label="Delete" emphasis="danger"{any} description="Deletes the marked files and folders, after asking"/>
    </hstack>
    <scroll grow="true">
      <vstack gap="none">
"#,
            up = off(!at_root),
            dir = escape(&self.dir.to_string_lossy()),
            one = off(marked == 1),
            any = off(marked > 0),
        );

        if self.entries.is_empty() {
            out.push_str("        <text color=\"muted\">This folder is empty.</text>\n");
        }
        for (index, entry) in self.entries.iter().enumerate() {
            let is_marked = self.marked.contains(&entry.name);
            let (label, description, what) = if entry.is_dir {
                (format!("{}/", entry.name), format!("Enters the folder {}", entry.name), "folder")
            } else {
                (
                    format!("{}  ({})", entry.name, human_size(entry.size)),
                    format!("Marks or unmarks the file {}", entry.name),
                    "file",
                )
            };
            let _ = write!(
                out,
                r#"        <hstack gap="sm">
          <checkbox id="mark-{index}"{checked} description="Marks the {what} {name} for rename, copy, move or delete"/>
          <item id="entry-{index}" grow="true" label="{label}"{selected} description="{description}"/>
        </hstack>
"#,
                checked = if is_marked { r#" checked="true""# } else { "" },
                what = what,
                name = escape(&entry.name),
                label = escape(&label),
                selected = if is_marked { r#" selected="true""# } else { "" },
                description = escape(&description),
            );
        }
        if self.truncated {
            let _ = writeln!(
                out,
                r#"        <text role="caption" color="muted">Showing the first {MAX_ENTRIES} entries.</text>"#
            );
        }

        let _ = write!(
            out,
            r#"      </vstack>
    </scroll>
    <hstack gap="sm">
      <button id="mark-all" label="{all}"{some} description="{all_description}"/>
      <text grow="true" role="caption" color="muted">{status}</text>
    </hstack>
  </vstack>
"#,
            all = if marked > 0 && marked == self.entries.len() { "Unmark all" } else { "Mark all" },
            some = off(!self.entries.is_empty()),
            all_description = if marked > 0 && marked == self.entries.len() {
                "Unmarks everything in this folder"
            } else {
                "Marks everything in this folder"
            },
            status = escape(&self.status),
        );

        // The dialog last, so it is in front. Its ids are its own; the
        // application never has to know them.
        if let Some(asking) = &self.asking {
            out.push_str(&match asking {
                Asking::CopyTo(dialog) | Asking::MoveTo(dialog) => dialog.render(),
                Asking::Delete(confirm) => confirm.render(),
                Asking::NewFolder(prompt) | Asking::Rename(prompt) => prompt.render(),
            });
        }
        out.push_str("</window>\n");
        out
    }
}

/// How a dialog was answered, flattened across the three kinds.
enum Outcome {
    Cancelled,
    Yes,
    Path(PathBuf),
    Text(String),
}

/// "welcome.txt", or "3 items".
fn describe(names: &BTreeSet<String>) -> String {
    match names.len() {
        1 => names.iter().next().cloned().unwrap_or_default(),
        n => format!("{n} items"),
    }
}

/// Why a name will not do, or nothing if it will.
fn bad_name(name: &str) -> Option<&'static str> {
    if name.is_empty() {
        Some("give it a name")
    } else if name.contains('/') {
        Some("a name cannot contain a slash")
    } else if name == "." || name == ".." {
        Some("that is not a name")
    } else {
        None
    }
}

fn leaf(path: &Path) -> String {
    path.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_owned()
}

/// Copy a file, or a folder and everything in it.
fn copy_recursive(from: &Path, to: &Path) -> std::io::Result<()> {
    if to.starts_with(from) {
        return Err(std::io::Error::other("cannot copy a folder into itself"));
    }
    if from.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_recursive(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(|_| ())
    }
}

/// Move by renaming, or by copying and removing when the destination is on
/// another mount, which `/run` is.
fn move_path(from: &Path, to: &Path) -> std::io::Result<()> {
    if to.starts_with(from) {
        return Err(std::io::Error::other("cannot move a folder into itself"));
    }
    match std::fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(err) if err.raw_os_error() == Some(18) => {
            copy_recursive(from, to)?;
            if from.is_dir() { std::fs::remove_dir_all(from) } else { std::fs::remove_file(from) }
        }
        Err(err) => Err(err),
    }
}

fn human_size(bytes: u64) -> String {
    if bytes < 1024 {
        format!("{bytes} B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

fn log(message: &str) {
    let line = format!("<6>awfiles: {message}\n");
    if let Ok(mut kmsg) = std::fs::OpenOptions::new().write(true).open("/dev/kmsg") {
        let _ = kmsg.write_all(line.as_bytes());
    } else {
        eprintln!("awfiles: {message}");
    }
}
