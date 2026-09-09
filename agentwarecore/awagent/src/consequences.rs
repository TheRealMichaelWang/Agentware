//! What a run of actions did, gathered so the model sees it in the same
//! message as the outcomes.
//!
//! An action is answered `done` the moment the haimanager performs it, which
//! is before the application has heard of it. The application's reply comes
//! later, as a fresh tree or a run of cells, and the haimanager turns that
//! into a notice on the agent's socket. So between the last outcome and the
//! next model exchange there is a gap in which the consequences are on their
//! way, and this module is what waits through it: for every application
//! acted on to have answered, and then for the answering to stop.
//!
//! **It waits for the answer, not for a clock.** A calculator answers a click
//! in a millisecond and the wait is a millisecond. A seventeen-character
//! `type-text` is seventeen events, seventeen re-renders and seventeen
//! notices, and the wait ends after the last of them rather than the first,
//! because a view read halfway through the typing would show the field with
//! half the text in it. [`CONSEQUENCES`] is the ceiling on all of it, for the
//! action that genuinely changes nothing an agent can see, since that case
//! has no signal at all.
//!
//! Then it turns the notices into what the model reads: a fresh view for an
//! interface that changed, and for a sheet the rectangle that changed and,
//! when that is a bounded amount, its present values. State, never a diff,
//! in both cases: the model is handed what is there now.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use awproto::agent::Notice;
use awproto::cells;
use awproto::turn;

use crate::{Agent, log};

/// The most an exchange waits for an action's consequences.
///
/// Reached only by an action nothing answers: a click on a control the
/// application ignores, a `select` of what is already selected. Everything
/// else ends the wait as soon as it has been answered.
pub const CONSEQUENCES: Duration = Duration::from_millis(800);

/// How long the socket has to stay silent, once everything acted on has
/// answered, for the answering to count as finished.
///
/// The gap between two re-renders of one application processing a burst of
/// events is a socket hop and a render, well under a millisecond, plus
/// whatever the haimanager was doing when the tree arrived, which can be a
/// frame's paint. Ten times a frame, so the burst is read as one.
pub const QUIET: Duration = Duration::from_millis(50);

/// The most cells a data change carries along with it.
///
/// A rectangle this size or smaller comes with its values, so the model sees
/// what the cells now hold without spending an exchange asking. Larger than
/// this, the rectangle is named and the model reads the part it wants; a
/// screenful of a grid is four hundred cells, and that is the size at which
/// cells stopped being sent as elements in the first place.
pub const CELLS_ATTACHED: u64 = 400;

/// Wait for what `expected` did to become visible, and hand over every
/// notice that arrived.
///
/// `expected` is the applications this exchange acted on and was told
/// `done`. The wait is over when each has answered at least once and the
/// socket has then been quiet for [`QUIET`], or when [`CONSEQUENCES`] runs
/// out, whichever is first. With nothing expected there is no wait: what is
/// already in the socket is taken and that is all.
pub fn await_notices(agent: &mut Agent, expected: &HashSet<String>) -> Vec<Notice> {
    let started = Instant::now();
    if !expected.is_empty() {
        let deadline = started + CONSEQUENCES;
        loop {
            let now = Instant::now();
            if now >= deadline {
                log(&format!(
                    "waited {}ms and {} never answered",
                    CONSEQUENCES.as_millis(),
                    unanswered(agent, expected).join(", ")
                ));
                break;
            }
            let all_answered = unanswered(agent, expected).is_empty();
            // Everything has answered: only a straggler could still be
            // coming, and a straggler is a re-render hot on the heels of
            // the last one. Nothing has: wait for it, as long as it takes.
            let patience = if all_answered { QUIET.min(deadline - now) } else { deadline - now };
            match agent.link.wait_notice(patience) {
                Ok(true) => continue,
                Ok(false) if all_answered => break,
                Ok(false) => continue,
                Err(err) => crate::connection_lost(&err),
            }
        }
    }
    agent.meter.waiting += started.elapsed();

    let notices = match agent.link.take_notices() {
        Ok(notices) => notices,
        Err(err) => crate::connection_lost(&err),
    };
    // Said in the log as words, one per notice, so a run can be read for
    // what the haimanager reported against what the model was then told.
    let said: Vec<String> = notices.iter().map(describe_briefly).collect();
    log(&format!(
        "notices after {}ms: {}",
        started.elapsed().as_millis(),
        if said.is_empty() { "none".to_owned() } else { said.join(", ") }
    ));
    notices
}

/// One notice as a few words for the log.
fn describe_briefly(notice: &Notice) -> String {
    match notice {
        Notice::View { instance } => format!("{instance} view"),
        Notice::Data { instance, source, element, range } => format!(
            "{instance} {source} {} {}",
            element.as_deref().unwrap_or("(no element)"),
            range.as_deref().unwrap_or("(whole)")
        ),
    }
}

/// The expected applications that have not said anything yet.
fn unanswered(agent: &Agent, expected: &HashSet<String>) -> Vec<String> {
    let mut missing: Vec<String> = expected
        .iter()
        .filter(|instance| {
            !agent.link.pending().iter().any(|notice| notice.instance() == instance.as_str())
        })
        .cloned()
        .collect();
    missing.sort();
    missing
}

/// The notices, as the model reads them: present views and present cells.
///
/// `None` when there is nothing to say. Views and cells are read out of the
/// haimanager's memory, which is why this can answer at all without asking
/// any application anything.
pub fn describe(agent: &mut Agent, notices: &[Notice]) -> Option<String> {
    if notices.is_empty() {
        return None;
    }
    let mut text = String::from(
        "The workspace changed while you worked. The present state, re-read for you:\n",
    );

    // One view per window, however many times it re-rendered. Kept by
    // handle as well as written out, because a sheet replaced whole is
    // sized by the `used` its element carries in the view.
    let mut views: Vec<(String, String)> = Vec::new();
    for notice in notices {
        let Notice::View { instance } = notice else { continue };
        if views.iter().any(|(seen, _)| seen == instance) {
            continue;
        }
        agent.say(turn::KIND_ACTION, &format!("re-reading {instance} (it changed)"));
        let asked = Instant::now();
        let answer = agent.link.view(instance);
        agent.meter.reading += asked.elapsed();
        match answer {
            Ok(markup) if !markup.is_empty() => {
                text.push('\n');
                text.push_str(&markup);
                text.push('\n');
                views.push((instance.clone(), markup));
            }
            Ok(_) => {}
            Err(err) => {
                log(&format!("could not re-read {instance}: {err}"));
                return Some(text);
            }
        }
    }

    for change in sheets_changed(notices) {
        text.push('\n');
        text.push_str(&change.describe(agent, &views));
        text.push('\n');
    }
    Some(text)
}

/// The `used` rectangle a view reports for one spreadsheet element: the
/// bounding box of everything published, which is what sizes a sheet that
/// was replaced whole.
///
/// `None` when the view has no such element; `Some(None)` when it has one
/// and the sheet is empty. The view is the reduced markup the haimanager
/// writes, one element per line, so the element's line is the one carrying
/// its id and the attribute is read off that line.
fn used_in(view: &str, element: &str) -> Option<Option<cells::Range>> {
    let id = format!(" id=\"{element}\"");
    let line = view
        .lines()
        .find(|line| line.trim_start().starts_with("<spreadsheet") && line.contains(&id))?;
    let used = line.split_once(" used=\"")?.1.split_once('"')?.0;
    Some(cells::parse_range(used))
}

/// One sheet's changes over an exchange, added up.
struct SheetChange {
    instance: String,
    source: String,
    element: Option<String>,
    /// The rectangle written, or `None` once any notice said the sheet was
    /// replaced whole: after that, naming a rectangle would be naming a
    /// part of a change that was not partial.
    range: Option<cells::Range>,
}

impl SheetChange {
    /// What the model is told, and, for a small enough rectangle, what is
    /// in it now.
    ///
    /// A sheet replaced whole has no rectangle of its own, so it is sized by
    /// the `used` its element reports in the view read this same round: a
    /// first keystroke into a new sheet is a snapshot of one cell, and one
    /// cell is worth attaching. `views` is what was read; a sheet whose view
    /// was not is named and left for `read_cells`.
    fn describe(&self, agent: &mut Agent, views: &[(String, String)]) -> String {
        let SheetChange { instance, source, element, range } = self;
        let shown = match element {
            Some(id) => format!("sheet {id} in {instance}"),
            None => format!("a sheet in {instance} that is not on a visible tab"),
        };
        let Some(id) = element else {
            return match range {
                Some(range) => format!(
                    "{shown} (source {source}) changed in cells {}.",
                    cells::range_name(*range)
                ),
                None => format!("{shown} (source {source}) was replaced whole."),
            };
        };
        let (range, how) = match range {
            Some(range) => (*range, format!("changed in cells {}", cells::range_name(*range))),
            None => {
                let used = views
                    .iter()
                    .find(|(seen, _)| seen == instance)
                    .and_then(|(_, view)| used_in(view, id));
                match used {
                    Some(Some(used)) => (used, "was replaced whole".to_owned()),
                    Some(None) => return format!("{shown} was replaced whole and is now empty."),
                    None => {
                        return format!(
                            "{shown} was replaced whole. Read the part you need with read_cells."
                        );
                    }
                }
            }
        };
        let named = cells::range_name(range);
        if cells::count(range) > CELLS_ATTACHED {
            return format!(
                "{shown} {how}; it now holds {named}, which is more than is attached here. \
                 Read the part you need with read_cells."
            );
        }
        agent.say(
            turn::KIND_ACTION,
            &format!("re-reading {instance} {id} {named} (it changed)"),
        );
        let asked = Instant::now();
        let answer = agent.link.cells(instance, id, &named);
        agent.meter.reading += asked.elapsed();
        match answer {
            Ok(rows) => format!("{shown} {how}. Cells {named} now hold:\n{rows}"),
            Err(err) => {
                log(&format!("could not re-read {instance} {id} {named}: {err}"));
                format!("{shown} {how}.")
            }
        }
    }
}

/// The data notices, one entry per sheet.
fn sheets_changed(notices: &[Notice]) -> Vec<SheetChange> {
    let mut sheets: Vec<SheetChange> = Vec::new();
    for notice in notices {
        let Notice::Data { instance, source, element, range } = notice else { continue };
        // A range the haimanager wrote is one this can read; anything else
        // is a wire fault, and the honest reading of it is "somewhere".
        let range = range.as_deref().and_then(cells::parse_range);
        match sheets
            .iter_mut()
            .find(|sheet| sheet.instance == *instance && sheet.source == *source)
        {
            Some(sheet) => {
                sheet.range = match (sheet.range, range) {
                    (Some(sum), Some(run)) => Some(cells::union(sum, run)),
                    _ => None,
                };
                // The latest word on which element shows it, since a tree
                // naming the element may have followed the first cells.
                if element.is_some() {
                    sheet.element = element.clone();
                }
            }
            None => sheets.push(SheetChange {
                instance: instance.clone(),
                source: source.clone(),
                element: element.clone(),
                range,
            }),
        }
    }
    sheets
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(instance: &str, source: &str, element: Option<&str>, range: Option<&str>) -> Notice {
        Notice::Data {
            instance: instance.into(),
            source: source.into(),
            element: element.map(str::to_owned),
            range: range.map(str::to_owned),
        }
    }

    /// Three runs into one sheet are one rectangle to read, and a sheet
    /// replaced whole stays replaced whole whatever was written after.
    #[test]
    fn a_sheets_changes_add_up() {
        let notices = [
            data("awsheet", "book/1", Some("sheet"), Some("B7")),
            data("awsheet", "book/1", Some("sheet"), Some("C7:D7")),
            Notice::View { instance: "awsheet".into() },
            data("awsheet", "book/1", Some("sheet"), Some("A8")),
            data("awsheet", "book/2", None, None),
            data("awsheet", "book/2", Some("sheet"), Some("A1")),
        ];
        let sheets = sheets_changed(&notices);
        assert_eq!(sheets.len(), 2);
        assert_eq!(sheets[0].range, Some(((0, 6), (3, 7))));
        assert_eq!(sheets[0].element.as_deref(), Some("sheet"));
        // Replaced whole, and the element named by the later tree is kept.
        assert_eq!(sheets[1].range, None);
        assert_eq!(sheets[1].element.as_deref(), Some("sheet"));
    }

    /// A sheet replaced whole is sized by what its element says is used,
    /// read off the view the same round attached.
    #[test]
    fn a_whole_sheet_is_sized_by_the_view() {
        let view = "<view app=\"awsheet\" desk=\"1\">\n  <text>Budget</text>\n  \
                    <spreadsheet id=\"sheet\" rows=\"1000\" columns=\"26\" cursor=\"A1\" \
                    selection=\"A1\" description=\"The grid\" used=\"A1:C5\" \
                    actions=\"focus select\"/>\n  \
                    <spreadsheet id=\"other\" rows=\"10\" columns=\"2\" cursor=\"A1\" \
                    description=\"Empty\" used=\"\" actions=\"focus select\"/>\n</view>";
        assert_eq!(used_in(view, "sheet"), Some(Some(((0, 0), (2, 4)))));
        assert_eq!(used_in(view, "other"), Some(None));
        assert_eq!(used_in(view, "nowhere"), None);
    }

    /// A rectangle a screenful large is named rather than attached: the
    /// limit is the same one that took cells out of the tree.
    #[test]
    fn the_attachment_limit_is_a_screenful() {
        assert!(cells::count(cells::parse_range("A1:T20").unwrap()) <= CELLS_ATTACHED);
        assert!(cells::count(cells::parse_range("A1:U20").unwrap()) > CELLS_ATTACHED);
    }
}
