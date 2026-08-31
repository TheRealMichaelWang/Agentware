//! Cut, copy and paste under the other mouse button.
//!
//! The clipboard and the selection are the compositor's: a run of text is
//! dragged out against its own copy of a control's value, and `Ctrl+C` never
//! reaches the application that owns the words. So the menu that acts on them
//! is the compositor's too, and it is the one thing the other button does not
//! hand to the application underneath.
//!
//! The split is by what was pressed. Text of any kind, a field, an editor, a
//! cell being typed into or a paragraph on a page, and this menu appears.
//! Anything else and the press goes on as a `context` event, which is what
//! lets a spreadsheet answer with its own menu on a cell. An application can
//! therefore never offer Copy on its own text, and does not need to: it has
//! no idea what is selected, because the selection was never told to it.
//!
//! It is AWML through the same parser, layout and painter as everything else:
//! one open `menu` with no label and three items, hung from the press exactly
//! as an application's context menu is hung, by the same `context_at`. There
//! is deliberately no second implementation of a floating panel.
//!
//! An agent sees none of it. It has no other button, it may read the
//! clipboard with a query, and what it wants written it writes with
//! `type-text`.

use std::os::fd::RawFd;

use crate::document::Document;
use crate::images::Images;
use crate::input::Key;
use crate::paint::font::Fonts;
use crate::paint::{Canvas, Rect};
use crate::ui::{self, Focus, Frame, Layout};

/// What the compositor's edit menu would offer over a point.
#[derive(Clone, Copy)]
pub struct Offer {
    /// Something is selected, so there is something to copy.
    pub selection: bool,
    /// The words can be changed, so cut and paste mean something.
    pub editable: bool,
}

/// Which text box the menu is acting on.
///
/// Chrome has text boxes too, and they are the same text box: the start
/// menu's prompt and the navigation bar's rename field run on the same
/// `text::Editing` an application's field does, so the same menu has to be
/// able to reach them. Naming the target here rather than assuming a client
/// is what makes that one menu instead of two.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Client(RawFd),
    /// The start menu's prompt.
    Prompt,
    /// The navigation bar's rename field.
    Rename,
}

pub struct EditMenu {
    doc: Document,
    layout: Layout,
    /// The panel's own rectangle, for deciding whether a press is on it.
    panel: Rect,
    /// What the menu is acting on, so what it decides goes back to the box
    /// that holds the words.
    pub target: Target,
}

impl EditMenu {
    /// Build the menu for one press, or `None` when it would offer nothing at
    /// all: a menu whose every item is dead is a menu not worth showing.
    pub fn open(
        fonts: &Fonts,
        screen: Rect,
        at: (i32, i32),
        offer: Offer,
        clipboard: bool,
        target: Target,
    ) -> Option<EditMenu> {
        let cut = offer.selection && offer.editable;
        let copy = offer.selection;
        let paste = offer.editable && clipboard;
        if !cut && !copy && !paste {
            return None;
        }

        let off = |on: bool| if on { "" } else { " disabled" };
        let markup = format!(
            "<window pad=\"none\">\
               <menu id=\"edit\" open=\"true\" description=\"What can be done with the words here\">\
                 <menuitem id=\"cut\" label=\"Cut\"{cut} description=\"Takes the selected words away and puts them on the clipboard\"/>\
                 <menuitem id=\"copy\" label=\"Copy\"{copy} description=\"Puts the selected words on the clipboard\"/>\
                 <menuitem id=\"paste\" label=\"Paste\"{paste} description=\"Puts what is on the clipboard in at the caret\"/>\
               </menu>\
             </window>",
            cut = off(cut),
            copy = off(copy),
            paste = off(paste),
        );

        let doc = Document::parse(&markup, 1).ok()?;
        let layout = ui::layout_menu_at(fonts, &doc, &Frame::Whole(screen), at);

        // The panel is where the items landed. Taken from the layout rather
        // than worked out again, so what the hand can press and what the eye
        // can see are the same rectangle.
        let items = &doc.tree.node(doc.tree.node(Document::ROOT).children[0]).children;
        let panel = items
            .iter()
            .map(|&item| layout.rect_of(item))
            .reduce(|a, b| {
                let x0 = a.x.min(b.x);
                let y0 = a.y.min(b.y);
                let x1 = (a.x + a.w).max(b.x + b.w);
                let y1 = (a.y + a.h).max(b.y + b.h);
                Rect::new(x0, y0, x1 - x0, y1 - y0)
            })?;

        Some(EditMenu { doc, layout, panel, target })
    }

    pub fn draw(&self, canvas: &mut Canvas, fonts: &Fonts, images: &Images) {
        // From the menu rather than from the root: the root is a window, and
        // a window paints its background across everything it was given,
        // which here is the screen.
        let menu = self.doc.tree.node(Document::ROOT).children[0];
        ui::paint_subtree(
            canvas,
            fonts,
            &ui::Content { images, sheets: &crate::sheet::Sheets::default() },
            &self.doc.tree,
            &self.layout,
            menu,
            &Focus::default(),
        );
    }

    pub fn contains(&self, x: i32, y: i32) -> bool {
        self.panel.contains(x, y)
    }

    /// What a press on the menu means, as the chord it stands for. The menu
    /// is a second way to say Ctrl+C, not a second implementation of it.
    pub fn press(&self, x: i32, y: i32) -> Option<Key> {
        let index = self.layout.hit_with_overlays(&self.doc.tree, x, y)?;
        match self.doc.tree.node(index).id()? {
            "cut" => Some(Key::Cut),
            "copy" => Some(Key::Copy),
            "paste" => Some(Key::Paste),
            _ => None,
        }
    }
}
