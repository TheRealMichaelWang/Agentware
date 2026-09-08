//! Layout: working out where everything goes.
//!
//! Reads a document and the metrics its parent defines, and answers with a
//! rectangle per node. Nothing here draws: `paint` is the other half, and it
//! reads the rectangles this produced rather than computing any of its own,
//! which is the whole reason these are two files rather than one.
//!
//! `use super::*` rather than a list of imports: this is a piece of its
//! parent, and the metrics and helpers it measures with are the parent's.

use super::*;

pub struct Layout {
    /// One rectangle per node, indexed as the arena is.
    pub rects: Vec<Rect>,
    /// The region each node is visible within, after clipping by every
    /// container above it.
    pub clips: Vec<Rect>,
    pub scrollers: Vec<Scroller>,
    /// The geometry of every spreadsheet in the document. Almost always
    /// empty, and never more than a handful.
    pub grids: Vec<Grid>,
}

impl Layout {
    /// A layout for a client that has not sent anything yet.
    pub fn empty() -> Layout {
        Layout {
            rects: Vec::new(),
            clips: Vec::new(),
            scrollers: Vec::new(),
            grids: Vec::new(),
        }
    }

    /// The innermost node at a point that an agent or a human could act on.
    ///
    /// Nothing behind an open dialog answers, however visible it is: that is
    /// what makes the dialog modal for the human, and the agent's view says
    /// the same thing about the same nodes.
    ///
    /// Walked in reverse so later siblings, which paint on top, win. Layout
    /// elements are skipped: clicking the gap between two buttons should hit
    /// nothing, not the stack that arranged them. A node scrolled outside its
    /// container is skipped too, because nothing is there to click.
    pub fn hit(&self, tree: &Tree, x: i32, y: i32) -> Option<usize> {
        (0..tree.nodes.len())
            .rev()
            .find(|&index| {
                tree.node(index).tag.is_control()
                    && !tree.blocked(index)
                    && !tree.folded(index)
                    && self.visible_at(index, x, y)
            })
    }

    /// The words under a point: the topmost `text` element with something in
    /// it, where nothing pressable is in the way.
    ///
    /// Separate from [`Layout::hit`] because that answers "what would a press
    /// act on", and static text is not something a press acts on. It is
    /// something a press can start selecting, which is a different question
    /// with a different answer, and folding them together would put a control
    /// and a caption in the same list.
    pub fn text_at(&self, tree: &Tree, x: i32, y: i32) -> Option<usize> {
        (0..tree.nodes.len()).rev().find(|&index| {
            tree.node(index).tag == Tag::Text
                && !tree.node(index).text.trim().is_empty()
                && !tree.blocked(index)
                && self.visible_at(index, x, y)
        })
    }

    /// The control under a point, with a dropdown's floating options taking
    /// precedence over whatever they hang across. Layout does not know what
    /// floats; the tree does, so callers pass it and this checks the floating
    /// nodes before the rest.
    pub fn hit_with_overlays(&self, tree: &Tree, x: i32, y: i32) -> Option<usize> {
        for select in tree.open_overlays().into_iter().rev() {
            if let Some(option) = tree
                .node(select)
                .children
                .iter()
                .rev()
                .copied()
                .find(|&child| !tree.node(child).disabled() && self.visible_at(child, x, y))
            {
                return Some(option);
            }
        }
        self.hit(tree, x, y)
    }

    /// Where a node is on screen, for driving the fake cursor to it.
    pub fn rect_of(&self, index: usize) -> Rect {
        self.rects[index]
    }

    /// Whether any part of a node is on screen.
    ///
    /// This is what an intent naming a node is checked against, so that an agent
    /// cannot act on something scrolled out of sight that no human could have
    /// clicked.
    pub fn is_visible(&self, index: usize) -> bool {
        self.clips[index].intersect(&self.rects[index]).is_some()
    }

    fn visible_at(&self, index: usize, x: i32, y: i32) -> bool {
        self.rects[index].contains(x, y) && self.clips[index].contains(x, y)
    }

    /// The innermost scroll container under a point, for routing the wheel.
    ///
    /// The agent never addresses one of these, which is why `scroll` needs no
    /// id: acting on a node inside one scrolls it, and the compositor
    /// works out what to move.
    pub fn scroller_at(&self, x: i32, y: i32) -> Option<&Scroller> {
        self.scrollers_at(x, y).next()
    }

    /// Every scroll container under a point, innermost first.
    ///
    /// The wheel wants the list, not just the innermost: a container that has
    /// nowhere further to go in the wheel's direction hands the notch to the
    /// one enclosing it, which is how a list inside a page scrolls the page
    /// once the list is at its end, and how a list that never overflowed
    /// does not swallow the wheel and leave the page stuck. Without this a
    /// pointer that landed on an inner container after the first notch made
    /// every notch after it do nothing.
    pub fn scrollers_at(&self, x: i32, y: i32) -> impl Iterator<Item = &Scroller> {
        self.scrollers
            .iter()
            .rev()
            .filter(move |scroller| self.visible_at(scroller.node, x, y))
    }
}

/// Lay out a document, resolving scroll offsets against `scroll` and clamping
/// them back into it.
///
/// The clamp belongs here because only layout knows how tall the content turned
/// out to be. A container whose content shrank should not stay scrolled past its
/// own end.
/// The compositor's own state that layout has to read.
///
/// All of it is ephemeral, none of it is in any tree, and it grew from one
/// map to four things, which is where a list of arguments stops being
/// readable. What they have in common is that the application neither sets
/// them nor is told about them.
pub struct Ephemeral<'a> {
    /// Offsets down, one per scroll container.
    pub scroll: &'a mut HashMap<String, i32>,
    /// Offsets across, which only a spreadsheet has.
    pub scroll_x: &'a mut HashMap<String, i32>,
    /// Column widths the human dragged, keyed by the element and the column's
    /// number in it.
    pub columns: &'a HashMap<String, i32>,
    /// Where the last right-press landed, while it still stands. An open menu
    /// hangs from here, which is what makes a context menu appear under the
    /// hand rather than wherever the application put the menu.
    pub context_at: Option<(i32, i32)>,
}

/// Lay out a document that cannot hold a table: the compositor's own chrome.
///
/// The navigation bar and the start menu are built here rather than by any
/// application, so they have neither an offset across nor a dragged column,
/// and saying that once is better than every caller carrying two maps that
/// stay empty.
pub fn layout_chrome(
    fonts: &Fonts,
    doc: &Document,
    frame: &Frame,
    scroll: &mut HashMap<String, i32>,
) -> Layout {
    let mut across = HashMap::new();
    let columns = HashMap::new();
    let mut state = Ephemeral {
        scroll,
        scroll_x: &mut across,
        columns: &columns,
        context_at: None,
    };
    layout(fonts, doc, frame, &mut state)
}

/// Chrome with a point for an open menu to hang from.
///
/// The compositor's own edit menu is a document like any other, and it hangs
/// where the other button was pressed through exactly the machinery an
/// application's context menu uses: one open `menu`, no label, and a point.
pub fn layout_menu_at(fonts: &Fonts, doc: &Document, frame: &Frame, at: (i32, i32)) -> Layout {
    let mut down = HashMap::new();
    let mut across = HashMap::new();
    let columns = HashMap::new();
    let mut state = Ephemeral {
        scroll: &mut down,
        scroll_x: &mut across,
        columns: &columns,
        context_at: Some(at),
    };
    layout(fonts, doc, frame, &mut state)
}

pub fn layout(fonts: &Fonts, doc: &Document, frame: &Frame, state: &mut Ephemeral) -> Layout {
    let count = doc.tree.nodes.len();
    let bounds = match frame {
        Frame::Whole(rect) => *rect,
        Frame::Regions(regions) => regions.background,
    };

    let mut placer = Placer {
        fonts,
        doc,
        scroll: state.scroll,
        scroll_x: state.scroll_x,
        columns: state.columns,
        context_at: state.context_at,
        rects: vec![Rect::new(0, 0, 0, 0); count],
        clips: vec![bounds; count],
        scrollers: Vec::new(),
        grids: Vec::new(),
    };

    match frame {
        Frame::Whole(rect) => placer.place(Tree::ROOT, *rect, *rect),
        Frame::Regions(regions) => placer.place_regions(*regions),
    }

    Layout {
        grids: placer.grids,
        rects: placer.rects,
        clips: placer.clips,
        scrollers: placer.scrollers,
    }
}

/// The height of one line of a node's own text.
pub(super) fn line_height(fonts: &Fonts, tree: &Tree, index: usize) -> i32 {
    fonts.line_height(&style_at(tree, index))
}

/// The height of the small label a group or list draws above its children.
pub(super) fn label_height(fonts: &Fonts, tree: &Tree, index: usize) -> i32 {
    let style = style_at(tree, index);
    fonts.line_height(&Style { size: style.size * 0.85, ..style }) + 6
}

pub(super) fn control_height(fonts: &Fonts, tree: &Tree, index: usize) -> i32 {
    line_height(fonts, tree, index) + control_pad() * 2
}

/// Elements that confine their children and so need their own clip.
pub(super) fn bounded(tag: Tag) -> bool {
    matches!(tag, Tag::Group | Tag::Dialog | Tag::List)
}

/// Elements that keep their children away from their own edges.
///
/// A window is padded but not framed: content should not sit flush against the
/// side of the screen, but the window itself draws nothing but background.
///
/// `pad="none"` opts out. It exists for documents whose frame is already exact,
/// which today means the compositor's own chrome: the navigation bar is sized to
/// its content and double-padding it pushed the buttons out of the bar.
pub(super) fn padded(node: &Node) -> bool {
    matches!(node.tag, Tag::Dialog | Tag::Window) && node.attr("pad") != Some("none")
}

/// Elements that confine their children to their own rectangle.
pub(super) fn clipping(tag: Tag) -> bool {
    bounded(tag) || tag == Tag::Scroll
}

/// Elements that draw their own label above their children, and so must reserve
/// room for it. Getting this list wrong does not fail loudly: the label simply
/// draws on top of the first child.
pub(super) fn titled(tag: Tag) -> bool {
    matches!(tag, Tag::Group | Tag::Dialog | Tag::List)
}

/// How big a node wants to be, given the width it will get.
pub(super) fn measure(fonts: &Fonts, tree: &Tree, index: usize, width: i32) -> i32 {
    let node = tree.node(index);

    // The vertical twin of the `width` hint: compositor-internal, physical
    // pixels, for chrome that knows the box it wants. The start menu's prompt
    // is the one user; a large box for a large ask.
    if let Some(height) = node.attr("height").and_then(|value| value.parse::<i32>().ok()) {
        return height.max(sc(20));
    }

    match node.tag {
        // Text wraps to the width it is given, so a paragraph in a narrow pane
        // is as tall as its lines rather than one line clipped at the edge.
        Tag::Text => {
            let style = style_at(tree, index);
            let lines = fonts.line_count(label_of(node), &style, width).max(1) as i32;
            lines * fonts.line_height(&style)
        }
        Tag::Icon => line_height(fonts, tree, index),
        Tag::Image => image_w().min(width.max(1)) * 9 / 16,
        // A rule across a column is a hairline tall; one down a row takes the
        // row's height and asks for none of its own.
        Tag::Divider if node.attr("dir") == Some("vertical") => 0,
        Tag::Divider => DIVIDER,
        // A tile is a button stood on end: icon above label, for a grid of
        // things to open. Compositor chrome for now, like `width`.
        Tag::Button if node.flag("tile") => {
            tile_pad() * 2 + tile_icon() + sc(6) + line_height(fonts, tree, index)
        }
        Tag::Button | Tag::Field | Tag::Item | Tag::Select => control_height(fonts, tree, index),
        // A menu is a control the width of its label; its items float and
        // take no room in the flow. One with no label draws nothing and
        // holds a small slot, so that it still has a rectangle an agent can
        // name and a place for its items to hang from.
        Tag::Menu | Tag::Tab => control_height(fonts, tree, index),
        Tag::MenuItem => 0,
        // A strip is a row of tabs plus the band that shows above and below
        // them. Measured off a tab rather than off the strip, because a tab
        // sets its own smaller type and the band is sized to what it holds.
        // A strip nobody sized: the tabs, a cap above and the same below.
        // Given a height instead, as the navigation bar gives one, the cap
        // is worked out from it.
        Tag::Tabs => tabs_row(fonts, tree, index) + tabs_cap_default() * 2,

        // A table is its header plus the rows it was given. It only ever
        // holds the window the application sent, so this is a small number
        // however large the sheet is; `grow` is how one asks for the room to
        // show more, and the row count it reports is what the bar is drawn
        // against.
        // A grid is as tall as there is room for it. It has no children to
        // measure and no natural height of its own: what it shows is however
        // much of the sheet fits, so a window opens with a screenful and
        // `grow` is how one asks for more.
        Tag::Spreadsheet => control_height(fonts, tree, index) * (SHEET_ROWS_MIN + 1),
        // Options take no room in the flow: they float below their dropdown
        // while it is open and are nowhere while it is not.
        Tag::Option => 0,
        Tag::Checkbox => control_height(fonts, tree, index).max(checkbox_size()),
        Tag::Slider => crate::slider::natural_height(),
        Tag::Editor => line_height(fonts, tree, index) * EDITOR_LINES + control_pad() * 2,

        // A row is as tall as its tallest child, each measured at the width
        // the row will actually give it: fixed children their natural width,
        // growers an equal share of what is left. Measuring every child at
        // the row's full width undercounted a paragraph in a narrow slot and
        // let it spill out of the row.
        Tag::HStack => {
            let gap = gap_of(node);
            let growers = node.children.iter().filter(|&&c| tree.node(c).flag("grow")).count() as i32;
            let fixed: i32 = node
                .children
                .iter()
                .filter(|&&c| !tree.node(c).flag("grow"))
                .map(|&c| natural_width(fonts, tree, c))
                .sum();
            let gaps = gap * (node.children.len().saturating_sub(1)) as i32;
            let each = if growers > 0 { ((width - fixed - gaps).max(0)) / growers } else { 0 };
            node.children
                .iter()
                .map(|&child| {
                    let slot = if tree.node(child).flag("grow") {
                        each
                    } else {
                        natural_width(fonts, tree, child)
                    };
                    measure(fonts, tree, child, slot)
                })
                .max()
                .unwrap_or(0)
        }

        // A scroll container that grows is a viewport onto its content, and a
        // viewport sized to less than a few rows is a viewport onto nothing.
        // It measures as its content, so a window fits it, but never as less
        // than a few rows, so a browser or a list opens with room to browse
        // in rather than as a slit the size of its first two entries.
        Tag::Scroll if node.flag("grow") => {
            let content = measure_children(fonts, tree, node, width);
            content.max(viewport_min())
        }

        // Everything else stacks vertically: the children's heights plus the
        // gaps between them, plus padding for anything that insets.
        //
        // A scroll container measures as its content, so one whose content fits
        // takes exactly the room it needs and never scrolls. It only scrolls
        // once something gives it less than that, which is what `grow` does.
        _ => {
            let padding = if padded(node) { padding() * 2 } else { 0 };
            let gap = gap_of(node);
            let height = measure_children(fonts, tree, node, width - padding);
            let title = if titled(node.tag) && node.attr("label").is_some() {
                label_height(fonts, tree, index) + gap
            } else {
                0
            };
            // The menu bar is a band above the padding rather than a child in
            // it, so a window that has one is that much taller.
            let bar = if has_menus(tree, index) { menu_band_h(fonts) } else { 0 };
            height + padding + title + bar
        }
    }
}

/// The height of a node's children stacked, with the gaps between them.
pub(super) fn measure_children(fonts: &Fonts, tree: &Tree, node: &Node, inner: i32) -> i32 {
    let gap = gap_of(node);
    let mut height = 0;
    let mut position = 0;
    for &child in &node.children {
        // A dialog floats over its window rather than stacking in it, and a
        // menu stands in the bar across the top of it, so neither adds
        // anything to the height of what is stacked.
        if node.tag == Tag::Window
            && matches!(tree.node(child).tag, Tag::Dialog | Tag::Menu)
        {
            continue;
        }
        if position > 0 {
            height += gap;
        }
        position += 1;
        height += measure(fonts, tree, child, inner);
    }
    height
}

pub(super) struct Placer<'a> {
    fonts: &'a Fonts,
    doc: &'a Document,
    scroll: &'a mut HashMap<String, i32>,
    /// Offsets across, for the one thing that scrolls that way.
    scroll_x: &'a mut HashMap<String, i32>,
    /// Column widths the human has dragged, in pixels, keyed by the column's
    /// identity. Ephemeral state like scroll and focus: the application
    /// declares what a column should start at and never hears that it moved,
    /// because where a column's edge sits is no more its business than where
    /// its window is.
    columns: &'a HashMap<String, i32>,
    /// Where an open menu should hang from, when a right-press put it there.
    context_at: Option<(i32, i32)>,
    rects: Vec<Rect>,
    clips: Vec<Rect>,
    scrollers: Vec<Scroller>,
    grids: Vec<Grid>,
}

impl Placer<'_> {
    /// Place each top-level child into the region it declares.
    ///
    /// A child that declares nothing, or declares something it may not have, is
    /// given an empty rectangle. It is then invisible and unreachable rather
    /// than being silently promoted to somewhere it does not belong, which is
    /// the failure that would be hardest to notice.
    fn place_regions(&mut self, regions: Regions) {
        self.rects[Tree::ROOT] = regions.background;
        self.clips[Tree::ROOT] = regions.background;

        let children = self.doc.tree.node(Tree::ROOT).children.clone();
        for child in children {
            let name = self.doc.tree.node(child).attr("region");
            let claim = regions.claim(name);
            match claim {
                // Inset, so a region's content does not sit flush against the
                // edge of the region. The compositor owns the division, so it
                // owns the breathing room too; there is no attribute an
                // agentdesk could set to take it back. Two exceptions, both
                // the compositor's: the background gets none, because what
                // goes there is a wallpaper and a wallpaper reaches the edges;
                // the taskbar gets less above and below, because it is one row
                // of controls in a band sized to hold exactly that.
                Some(rect) => {
                    let inner = match name {
                        Some("background") => rect,
                        Some("taskbar") => Rect::new(
                            rect.x + padding(),
                            rect.y + taskbar_inset(),
                            rect.w - padding() * 2,
                            rect.h - taskbar_inset() * 2,
                        ),
                        _ => rect.inset(padding()),
                    };
                    self.place(child, inner, rect)
                }
                None => {
                    self.rects[child] = Rect::new(0, 0, 0, 0);
                    self.clips[child] = Rect::new(0, 0, 0, 0);
                }
            }
        }
    }

    fn place(&mut self, index: usize, area: Rect, clip: Rect) {
        let tree = &self.doc.tree;
        self.rects[index] = area;
        self.clips[index] = clip;

        let node = tree.node(index);
        let tag = node.tag;
        let padding = if padded(node) { padding() } else { 0 };
        let mut inner = area.inset(padding);

        // A container that labels itself takes the top of the space before the
        // children divide what is left.
        if titled(tag) && node.attr("label").is_some() {
            let used = label_height(self.fonts, tree, index) + gap_of(node);
            inner = Rect::new(inner.x, inner.y + used, inner.w, inner.h - used);
        }

        let gap = gap_of(node);
        let mut children = node.children.clone();
        let inside = if clipping(tag) {
            clip.intersect(&area).unwrap_or(Rect::new(area.x, area.y, 0, 0))
        } else {
            clip
        };

        // Two of a window's children are not in its flow, and neither of them
        // is placed by the application.
        //
        // Its **menus** are its menu bar: one row across the top of the
        // window, under the title bar the compositor draws and above
        // everything the application put inside. An application says it has
        // an Edit menu; where a menu bar goes is not a thing an application
        // gets an opinion about, which is why the parser refuses a `menu`
        // anywhere else.
        //
        // Its **dialogs** float over the content, centred, so an application
        // opens one by adding it to its tree and closes one by leaving it
        // out, and never says where it goes.
        if tag == Tag::Window {
            let menus: Vec<usize> = children
                .iter()
                .copied()
                .filter(|&child| tree.node(child).tag == Tag::Menu)
                .collect();
            let dialogs: Vec<usize> = children
                .iter()
                .copied()
                .filter(|&child| tree.node(child).tag == Tag::Dialog)
                .collect();
            children.retain(|child| !matches!(tree.node(*child).tag, Tag::Dialog | Tag::Menu));

            let mut inner = inner;
            if !menus.is_empty() {
                let band = menu_band(self.fonts, area);
                let row = Rect::new(
                    band.x + menu_lead(),
                    band.y,
                    (band.w - menu_lead() * 2).max(0),
                    band.h,
                );
                self.place_row(&menus, row, menu_gap(), inside);
                // The content starts under the bar, and is padded from there
                // as it would have been from the top of the window.
                let below = Rect::new(area.x, band.y + band.h, area.w, (area.h - band.h).max(0));
                inner = below.inset(padding);
            }

            self.place_column(&children, inner, gap, inside);
            for dialog in dialogs {
                let w = dialog_w().min(inner.w);
                let h = measure(self.fonts, &self.doc.tree, dialog, w).min(inner.h);
                let rect = Rect::new(inner.x + (inner.w - w) / 2, inner.y + (inner.h - h) / 2, w, h);
                self.place(dialog, rect, inside);
            }
            return;
        }

        // A strip of tabs: each at its own width, left to right, the way a
        // row lays out fixed children, inside a band that stands a little
        // taller than they do.
        if tag == Tag::Tabs {
            // The frame between the caps is what the navigation bar handed
            // its buttons, measured off the band rather than off the strip's
            // own box so that the first tab starts where the band does. An
            // application's strip sits inside a padded window and its band
            // already reaches the window's edges; measuring from the box
            // left its tabs a margin the bar does not have.
            let frame = tabs_frame(self.fonts, inner, inside);
            // The row inside it is a tab's own height, not the frame's, and
            // the frame is then a *clip* over it. Sizing the row to the frame
            // instead changes the lighting by a step per row: it looks the
            // same and is not.
            let room = Rect::new(
                frame.x,
                frame.y,
                frame.w,
                tabs_row(self.fonts, &self.doc.tree, index),
            );
            let cut = inside.intersect(&frame).unwrap_or(Rect::new(frame.x, frame.y, 0, 0));
            self.place_row(&children, room, gap, cut);
            return;
        }

        // A dropdown's options and a menu's items are not in the flow. Open,
        // they hang below in a column, over whatever is there, and above
        // instead if the window has no room below. Closed, they are nowhere:
        // an empty rectangle, so nothing paints them and nothing hits them.
        if tag == Tag::Select || tag == Tag::Menu {
            let open = node.flag("open");
            let row = control_height(self.fonts, tree, index);
            // A dropdown's list is the width of its box, because the box is
            // showing one of the same values. A menu's is the width of its
            // widest item, because a menu button says "Edit" and its items
            // say things like "Paste as values".
            let width = if tag == Tag::Menu {
                children
                    .iter()
                    .map(|&child| natural_width(self.fonts, tree, child))
                    .max()
                    .unwrap_or(0)
                    .max(area.w)
            } else {
                area.w
            };
            let count = children.len() as i32;
            // A menu the human opened with the other button hangs from where
            // they pressed; everything else hangs from its own box. Nothing
            // in the tree says which, because the compositor is what saw the
            // press.
            let (anchor_x, above, below) = match self.context_at.filter(|_| tag == Tag::Menu) {
                Some((x, y)) => (x, y, y),
                None => (area.x, area.y, area.y + area.h),
            };
            // Floating over what follows means escaping the container's clip:
            // the options answer to the document's, not to the group or list
            // the box happens to sit in. Clipping them locally silently ate
            // every option past a short container's edge, which the theme
            // dropdown found by living in a group exactly one row tall.
            let float_clip = self.clips[Tree::ROOT];
            let fits_below = below + row * count <= float_clip.y + float_clip.h;
            let top = if fits_below || above - row * count < float_clip.y {
                below
            } else {
                above - row * count
            };
            for (at, child) in children.into_iter().enumerate() {
                let rect = if open {
                    Rect::new(anchor_x, top + row * at as i32, width, row)
                } else {
                    Rect::new(0, 0, 0, 0)
                };
                self.rects[child] = rect;
                self.clips[child] = if open { float_clip } else { Rect::new(0, 0, 0, 0) };
            }
            return;
        }

        if tag == Tag::Spreadsheet {
            self.place_sheet(index, inner, inside);
            return;
        }

        if tag == Tag::HStack {
            self.place_row(&children, inner, gap, inside);
            return;
        }

        if tag == Tag::Scroll {
            self.place_scroll(index, &children, inner, gap, inside);
            return;
        }

        self.place_column(&children, inner, gap, inside);
    }

    /// Children keep their natural width and share the height. A child marked
    /// `grow` takes what is left over, which is how a field sits beside a
    /// fixed-width button.
    fn place_row(&mut self, children: &[usize], inner: Rect, gap: i32, clip: Rect) {
        let tree = &self.doc.tree;
        let mut fixed = 0;
        let mut growers = 0;
        for &child in children {
            if tree.node(child).flag("grow") {
                growers += 1;
            } else {
                fixed += natural_width(self.fonts, tree, child);
            }
        }

        let gaps = gap * (children.len().saturating_sub(1)) as i32;
        let spare = (inner.w - fixed - gaps).max(0);
        let each = if growers > 0 { spare / growers } else { 0 };

        let mut x = inner.x;
        for &child in children {
            let node = self.doc.tree.node(child);
            let width = if node.flag("grow") {
                each
            } else {
                natural_width(self.fonts, &self.doc.tree, child)
            };
            // A one-line control in a taller row keeps its own height and
            // sits in the middle of the row, rather than being stretched to
            // the height of whatever paragraph or stack it shares the row
            // with. Containers and text take the row: they have insides that
            // want the room, or centre themselves when painted.
            let mut slot = Rect::new(x, inner.y, width, inner.h);
            if matches!(node.tag, Tag::Button | Tag::Field | Tag::Select | Tag::Checkbox | Tag::Slider) {
                // A control with a border all the way round is kept inside
                // what is actually visible, as well as inside the row. It
                // only ever differs in a strip of tabs, where the row is a
                // tab's height and the frame clipping it is shorter: a tab
                // with no bottom edge is the whole point of a tab, and a box
                // with no bottom edge is a bug. The navigation bar's rename
                // field is where it showed.
                let bounded = if matches!(node.tag, Tag::Field | Tag::Select) {
                    inner.h.min((clip.y + clip.h - inner.y).max(0))
                } else {
                    inner.h
                };
                let own = measure(self.fonts, &self.doc.tree, child, width).min(bounded);
                slot = Rect::new(x, inner.y + (bounded - own) / 2, width, own);
            }
            self.place(child, slot, clip);
            x += width + gap;
        }
    }

    /// Children take their measured height, top to bottom. A child marked `grow`
    /// takes what is left instead, which may be *less* than it asked for: that is
    /// what gives a scroll container a bounded viewport and something to scroll.
    fn place_column(&mut self, children: &[usize], inner: Rect, gap: i32, clip: Rect) {
        let heights = self.column_heights(children, inner, gap);

        let mut y = inner.y;
        for (&child, height) in children.iter().zip(heights) {
            self.place(child, Rect::new(inner.x, y, inner.w, height), clip);
            y += height + gap;
        }
    }

    fn column_heights(&self, children: &[usize], inner: Rect, gap: i32) -> Vec<i32> {
        let tree = &self.doc.tree;
        let natural: Vec<i32> = children
            .iter()
            .map(|&child| measure(self.fonts, tree, child, inner.w))
            .collect();

        let growers = children.iter().filter(|&&c| tree.node(c).flag("grow")).count();
        if growers == 0 {
            return natural;
        }

        let gaps = gap * (children.len().saturating_sub(1)) as i32;
        let fixed: i32 = children
            .iter()
            .zip(&natural)
            .filter(|&(&child, _)| !tree.node(child).flag("grow"))
            .map(|(_, &height)| height)
            .sum();
        let each = (inner.h - fixed - gaps).max(0) / growers as i32;

        children
            .iter()
            .zip(&natural)
            .map(|(&child, &height)| if tree.node(child).flag("grow") { each } else { height })
            .collect()
    }

    /// Place a grid: a header that does not scroll down, a row-label gutter
    /// that does not scroll across, and the window of rows the application
    /// sent.
    ///
    /// Two axes with two different owners, which is the whole of the design.
    /// A spreadsheet: a header of column letters, a gutter of row numbers,
    /// and a grid of cells that are not nodes.
    ///
    /// Nothing here is placed in the arena except the element itself. What
    /// comes out instead is one [`Grid`], which is arithmetic: where a cell is
    /// and which cell a point is over are both computed from it, by painting
    /// and by hit testing, from the same numbers. That is what makes a sheet
    /// of a million cells cost the same as a sheet of ten.
    fn place_sheet(&mut self, index: usize, area: Rect, clip: Rect) {
        let doc = self.doc;
        let tree = &doc.tree;
        let node = tree.node(index);

        let source = node.attr("source").unwrap_or_default().to_owned();
        let row_h = control_height(self.fonts, tree, index);
        // Bounded, because these are numbers a client chose and the arithmetic
        // below runs per frame. A sheet wider than this is not a sheet.
        let (columns, rows) = sheet_shape(node);

        // The gutter is as wide as the largest row number it will ever show,
        // so it does not change width as the sheet is scrolled.
        let label_style = Style { size: style_at(tree, index).size * 0.9, ..style_at(tree, index) };
        let gutter_w = self.fonts.measure(&rows.to_string(), &label_style) + control_pad() * 2;
        let width = character_width(self.fonts, tree, index) * DEFAULT_COLUMN_CHARS
            + control_pad() * 2;

        let key = doc.key(index).to_owned();
        let wide: Vec<(u32, i32)> = {
            let mut wide: Vec<(u32, i32)> = self
                .columns
                .iter()
                .filter_map(|(at, &w)| {
                    let column = at.strip_prefix(&format!("{key}#"))?.parse::<u32>().ok()?;
                    (column < columns).then_some((column, w))
                })
                .collect();
            wide.sort_unstable();
            wide
        };

        self.rects[index] = area;
        self.clips[index] = clip;

        let header = Rect::new(area.x + gutter_w, area.y, (area.w - gutter_w).max(0), row_h);
        let body = Rect::new(
            area.x + gutter_w,
            area.y + row_h,
            (area.w - gutter_w).max(0),
            (area.h - row_h).max(0),
        );
        let gutter = Rect::new(area.x, area.y + row_h, gutter_w, body.h);

        let mut grid = Grid {
            node: index,
            source,
            body,
            header,
            gutter,
            row_h,
            rows,
            columns,
            offset: (0, 0),
            width,
            wide,
        };

        // Both offsets are the compositor's, and both are clamped here
        // because only layout knows how big the content turned out to be.
        let (content_w, content_h) = grid.content();
        let across = self
            .scroll_x
            .get(&key)
            .copied()
            .unwrap_or(0)
            .clamp(0, (content_w - body.w).max(0));
        let down = self
            .scroll
            .get(&key)
            .copied()
            .unwrap_or(0)
            .clamp(0, (content_h - body.h).max(0));
        self.scroll_x.insert(key.clone(), across);
        self.scroll.insert(key, down);
        grid.offset = (across, down);

        if content_h > body.h {
            self.scrollers.push(Scroller {
                node: index,
                content: content_h,
                viewport: body.h,
                offset: down,
                horizontal: false,
                permanent: true,
                step: row_h,
            });
        }
        if content_w > body.w {
            self.scrollers.push(Scroller {
                node: index,
                content: content_w,
                viewport: body.w,
                offset: across,
                horizontal: true,
                permanent: false,
                step: width,
            });
        }

        self.grids.push(grid);
    }

    /// Content is laid out at its natural height and translated up by the offset,
    /// so every rectangle in the arena is already in screen coordinates. Hit
    /// testing then needs no special case for scrolled content, and neither does
    /// the fake cursor: a node's rectangle is where it is, or it is outside its
    /// clip and cannot be reached at all.
    fn place_scroll(&mut self, index: usize, children: &[usize], inner: Rect, gap: i32, clip: Rect) {
        let tree = &self.doc.tree;
        let heights: Vec<i32> = children
            .iter()
            .map(|&child| measure(self.fonts, tree, child, inner.w))
            .collect();

        let gaps = gap * (children.len().saturating_sub(1)) as i32;
        let content = heights.iter().sum::<i32>() + gaps;
        let furthest = (content - inner.h).max(0);

        // `anchor="end"` is a transcript's request: keep the end in view as
        // content grows, until the human scrolls away from it. The offset map
        // holds AT_END for such a container while it is at its end, so that
        // the next layout, with more content, resolves it to the new end
        // rather than to a number that used to be the end. A concrete offset
        // is what the human's wheel writes, and it means "here, not the end".
        let key = self.doc.key(index).to_owned();
        let follows_end = tree.node(index).attr("anchor") == Some("end");
        let offset = match self.scroll.get(&key).copied() {
            Some(AT_END) => furthest,
            Some(stored) => stored.clamp(0, furthest),
            None if follows_end => furthest,
            None => 0,
        };
        self.scroll.insert(key, if follows_end && offset >= furthest { AT_END } else { offset });
        self.scrollers.push(Scroller {
            node: index,
            content,
            viewport: inner.h,
            offset,
            horizontal: false,
            permanent: false,
            step: 1,
        });

        let mut y = inner.y - offset;
        for (&child, height) in children.iter().zip(heights) {
            self.place(child, Rect::new(inner.x, y, inner.w, height), clip);
            y += height + gap;
        }
    }
}

/// How tall a whole document wants to be at a given width.
///
/// This is the measure pass run from the root, and it exists for exactly one
/// caller: sizing a window to its content when the first tree arrives. A scroll
/// container measures as its full content here, which is what a clamp against
/// the workspace is for; the container only actually scrolls once layout gives
/// it less room than it asked.
pub fn natural_height(fonts: &Fonts, tree: &Tree, width: i32) -> i32 {
    measure(fonts, tree, Tree::ROOT, width)
}

/// How wide a whole document wants to be so that nothing in it is squeezed.
///
/// The width sibling of [`natural_height`], for the same one caller: sizing a
/// window to its content when the first tree arrives. AWML describes
/// affordances rather than arrangement and has no width to declare, so this
/// is derived: a row wants the sum of what its children want, a column wants
/// the widest of them, a control wants its label and its padding, and text
/// wants its own length up to a cap, because text wraps and a paragraph must
/// not size a window to its longest line. A spacer wants nothing. What comes
/// out is the narrowest width at which every control sits inside its
/// container at its natural size, which is what a window should open at.
pub fn document_width(fonts: &Fonts, tree: &Tree) -> i32 {
    wanted_width(fonts, tree, Tree::ROOT)
}

/// The strip above the tabs, and the strip below them before the hairline.
///
/// Both taken off the navigation bar, read out of a capture a pixel at a
/// time, because guessing at this produced the wrong answer twice. Down a
/// column through one of its tabs the bar is: three rows of `raised`, then
/// the tab itself standing on `background`, then two more rows of `raised`,
/// then one row of `border`.
///
/// The important half is what the tabs stand on. They stand on the same
/// colour as the desk behind the bar, not on a raised band, and the raised
/// rows are thin edges above and below. Painting a full band and putting
/// tabs on top of it is what makes them look like buttons lying on a bar
/// rather than tabs cut into one.
/// How much raised shows above the tabs, and so below them too.
///
/// Derived from the height the strip was given rather than fixed, because
/// that is what the navigation bar did: it worked its inset out from the bar
/// it had to fill, so the arithmetic came out differently at every interface
/// scale. Fixed constants agreed with it at 1.0 and at nothing else, which
/// is a whole class of bug this codebase has a rule against and I wrote
/// anyway: every metric is a logical size through `sc`, and a *derived* one
/// stays derived.
///
/// The `+ 12` is unscaled and the size is the small one, both copied from
/// the bar verbatim. They are not what anyone would write now; they are what
/// the bar is, and the bar is not to change.
pub(super) fn tabs_cap(fonts: &Fonts, height: i32) -> i32 {
    let row = fonts.line_height(&Style { size: 11.0 * scale(), ..Style::default() }) + 12;
    ((height - row) / 2).max(2)
}

/// The natural cap, for a strip nobody has given a height to.
pub(super) fn tabs_cap_default() -> i32 { sc(3) }

/// How tall the tabs in a strip stand. Measured off a tab rather than off
/// the strip, since a tab sets its own smaller type.
pub(super) fn tabs_row(fonts: &Fonts, tree: &Tree, strip: usize) -> i32 {
    tree.node(strip)
        .children
        .iter()
        .copied()
        .find(|&child| tree.node(child).tag == Tag::Tab)
        .map(|tab| control_height(fonts, tree, tab))
        .unwrap_or_else(|| control_height(fonts, tree, strip))
}
/// The margin before the first tab and after the last. The band runs to the
/// edges under it; the tabs do not start there.
///
/// Unscaled, like the `+ 12` in [`tabs_cap`], and for the same reason: the
/// navigation bar's frame was inset by a raw ten pixels, so scaling this
/// agreed with it at 1.0 and nowhere else.
pub(super) fn tabs_lead() -> i32 { 10 }

/// The band a strip paints: the width of whatever it is clipped to rather
/// than of its own box. A row of tabs standing in the middle of a window
/// with a margin either side is a row of buttons.
pub(super) fn tabs_band(rect: Rect, clip: Rect) -> Rect {
    Rect::new(clip.x, rect.y, clip.w, rect.h)
}

/// The frame the tabs stand in: the band, inset by the cap above and below
/// and the margin at each end.
///
/// One function because the band and the tabs have to agree about where the
/// strip begins. They did not when the band was drawn across the clip and
/// the tabs were laid out from the strip's box, which is a difference only
/// an application ever saw, since the bar's box is the bar.
pub(super) fn tabs_frame(fonts: &Fonts, rect: Rect, clip: Rect) -> Rect {
    let band = tabs_band(rect, clip);
    let cap = tabs_cap(fonts, band.h);
    Rect::new(
        band.x + tabs_lead(),
        band.y + cap,
        (band.w - tabs_lead() * 2).max(0),
        (band.h - cap * 2).max(0),
    )
}

/// Which slot a tab dragged to `x` belongs in, given every tab's rectangle
/// in display order and which of them is the one being carried.
///
/// The navigation bar's rule, shared with an application's strip because
/// there is no second way to do this that is worth having: count the other
/// tabs whose middle the pointer has passed. It holds no state, so several
/// motions arriving between two repaints all agree, which is what a paced
/// test never catches and mouse speed always does.
pub fn tab_slot(rects: &[Rect], held: usize, x: i32) -> usize {
    rects
        .iter()
        .enumerate()
        .filter(|(at, _)| *at != held)
        .filter(|(_, rect)| x > rect.x + rect.w / 2)
        .count()
}

/// Where the cross that closes a tab is drawn, measured in from the right.
pub fn tab_close_w() -> i32 { sc(20) }

/// A closable tab's label with the room for its cross on the end.
///
/// Four spaces, which is not a measurement anyone would choose: it is what
/// the navigation bar reserved before this element existed, where the room
/// was four literal spaces written into the label. Measured as one string
/// rather than added on afterwards, because a proportional font's advances
/// do not accumulate the same way and the two differ by a pixel.
pub(super) fn tab_label_room(label: &str, closable: bool) -> String {
    if closable { format!("{label}    ") } else { label.to_owned() }
}

/// The cross at a tab's right end.
///
/// Lives here rather than in the navigation bar because the bar is not the
/// only thing with tabs any more: an application's strip draws the same
/// cross, and one function is what keeps them the same cross.
pub fn draw_tab_close(canvas: &mut Canvas, tab: Rect, active: bool) {
    let cx = (tab.x + tab.w - tab_close_w() / 2 - sc(2)) as f32;
    let cy = (tab.y + tab.h / 2) as f32;
    let r = sc(3) as f32;
    let ink = if active { text() } else { muted() };
    let thickness = sc(1).max(1);
    canvas.stroke_line(cx - r, cy - r, cx + r, cy + r, thickness, ink);
    canvas.stroke_line(cx - r, cy + r, cx + r, cy - r, thickness, ink);
}

/// Draw a tab being carried: blank where it rests, and the tab itself riding
/// under the pointer.
///
/// **This is what a tab drag's feedback is**, and both strips need it for the
/// same reason. Without it the only sign anything is happening is the row
/// rearranging once the pointer crosses a neighbour's midpoint, which for the
/// first half of any drag is no feedback at all: the mechanism works and the
/// interaction reads as dead. The navigation bar had this and an application's
/// strip did not, which is the whole of why one felt worse than the other.
///
/// It is here rather than in either caller because two of it would drift, and
/// this is exactly the thing the tabs element was collapsed into one
/// implementation to stop happening.
pub fn draw_tab_ghost(
    canvas: &mut Canvas,
    fonts: &Fonts,
    home: Rect,
    ghost: Rect,
    label: &str,
    active: bool,
    closable: bool,
) {
    // Blank the tab's resting place so it reads as picked up. The colour is
    // the one the strip stands on rather than the raised one, which left a
    // grey patch where the row's own background should be.
    canvas.fill_rect(home, background());

    canvas.shadow(ghost, radius_control(), sc(8), 110);
    canvas.fill_round_rect(ghost, radius_control(), if active { accent() } else { pressed() });
    let style = Style { size: 11.0 * scale(), ..Style::default() };
    canvas.clipped(ghost.inset(2), |canvas| {
        canvas.draw_text(
            fonts,
            label,
            ghost.x + sc(12),
            ghost.y + (ghost.h - fonts.line_height(&style)) / 2,
            &style,
            text(),
        );
    });
    if closable {
        draw_tab_close(canvas, ghost, active);
    }
}

/// Room at a dropdown's right end for the chevron that says it opens.
pub(super) fn chevron_w() -> i32 { sc(22) }
/// Whether a document's top-level content asks to fill whatever it is given.
///
/// An application marks a container `grow` to say "this takes the room": a
/// browser's listing, a settings page, a transcript. When that mark is on
/// something directly under the window, the application is saying the window
/// as a whole should have room, and the compositor opens it with some rather
/// than at the size of its first tree. A calculator marks nothing at that
/// level and opens the size of a calculator.
pub fn wants_room(tree: &Tree) -> bool {
    tree.node(Tree::ROOT)
        .children
        .iter()
        .any(|&child| tree.node(child).flag("grow") && tree.node(child).tag != Tag::Dialog)
}

/// The most a run of text asks a window for. Longer text wraps.
pub(super) fn text_cap() -> i32 { sc(320) }
/// The least a growing scroll container measures as: a few rows of content.
pub(super) fn viewport_min() -> i32 { sc(180) }

pub(super) fn wanted_width(fonts: &Fonts, tree: &Tree, index: usize) -> i32 {
    let node = tree.node(index);
    let style = style_at(tree, index);
    let children = |floating: bool| {
        node.children
            .iter()
            .copied()
            .filter(move |&child| {
                !(floating && matches!(tree.node(child).tag, Tag::Dialog | Tag::Menu))
            })
            .map(|child| wanted_width(fonts, tree, child))
    };
    match node.tag {
        Tag::Text | Tag::Icon => {
            let label = label_of(node);
            if label.trim().is_empty() {
                0
            } else {
                fonts.measure(label, &style).min(text_cap())
            }
        }
        Tag::Divider if node.attr("dir") == Some("vertical") => DIVIDER,
        Tag::Divider => 0,
        Tag::Image | Tag::Button | Tag::Field | Tag::Editor | Tag::Checkbox | Tag::Slider | Tag::Item | Tag::Select => {
            natural_width(fonts, tree, index)
        }
        Tag::Option => 0,
        // A screenful of columns, not the whole sheet: a grid is wider than
        // any window and the bar across the bottom is what says so.
        Tag::Spreadsheet => {
            (character_width(fonts, tree, index) * DEFAULT_COLUMN_CHARS + control_pad() * 2)
                * SHEET_COLUMNS_MIN
        }
        Tag::MenuItem => 0,
        Tag::Menu | Tag::Tab | Tag::Tabs => natural_width(fonts, tree, index),
        Tag::HStack => {
            let gaps = gap_of(node) * (node.children.len().saturating_sub(1)) as i32;
            children(false).sum::<i32>() + gaps
        }
        // A dialog floats and sizes itself, so it asks the window for
        // nothing; the window's own content is what the window is for. The
        // menus are a row rather than one of the stacked children, so they
        // ask for their sum, and the window is at least wide enough to show
        // its own menu bar.
        Tag::Window => {
            let widest = children(true).max().unwrap_or(0);
            let content = widest + if padded(node) { padding() * 2 } else { 0 };
            content.max(menu_bar_width(fonts, tree, index))
        }
        Tag::Dialog => children(false).max().unwrap_or(0) + padding() * 2,
        _ => children(false).max().unwrap_or(0),
    }
}

/// How wide a node is when it is not being stretched.
pub(super) fn natural_width(fonts: &Fonts, tree: &Tree, index: usize) -> i32 {
    let node = tree.node(index);
    let style = style_at(tree, index);

    // A compositor-internal sizing hint, in physical pixels, not part of the
    // application catalogue. It exists for chrome the compositor builds about
    // geometry it already knows: the tab rename field takes the width of the
    // tab it replaces instead of jumping to the fallback below.
    if let Some(width) = node.attr("width").and_then(|value| value.parse::<i32>().ok()) {
        return width.max(sc(40));
    }

    match node.tag {
        Tag::Text | Tag::Icon => fonts.measure(label_of(node), &style),
        Tag::Image => image_w(),
        Tag::Divider if node.attr("dir") == Some("vertical") => DIVIDER,
        Tag::Button => {
            // A glyph button is a small square: the shape is the label.
            if node.attr("glyph").is_some() {
                return fonts.line_height(&style) + control_pad() * 2;
            }
            let icon = if node.attr("icon").is_some() && !node.flag("tile") {
                button_icon() + sc(6)
            } else {
                0
            };
            fonts.measure(label_of(node), &style) + button_pad() * 2 + icon
        }
        Tag::Checkbox => checkbox_size() + 8 + fonts.measure(label_of(node), &style),
        Tag::Slider => crate::slider::natural_width(),
        Tag::Item | Tag::Option | Tag::MenuItem => {
            fonts.measure(label_of(node), &style) + control_pad() * 2
        }
        Tag::Tab => {
            let room = tab_label_room(label_of(node), node.flag("closable"));
            fonts.measure(&room, &style) + button_pad() * 2
        }
        // A menu with no label is a place for its items to hang from and
        // nothing else, so it asks for almost nothing.
        Tag::Menu => {
            let label = label_of(node);
            if label.is_empty() {
                sc(2)
            } else {
                fonts.measure(label, &style) + button_pad() * 2
            }
        }
        Tag::Tabs => {
            let gap = gap_of(node);
            node.children
                .iter()
                .map(|&child| natural_width(fonts, tree, child))
                .sum::<i32>()
                + gap * (node.children.len().saturating_sub(1)) as i32
        }
        // Wide enough for its widest option, so choosing one never changes
        // the width of the row it sits in.
        Tag::Select => {
            let widest = node
                .children
                .iter()
                .map(|&child| fonts.measure(label_of(tree.node(child)), &style))
                .max()
                .unwrap_or(0)
                .max(fonts.measure(node.attr("placeholder").unwrap_or(""), &style));
            widest + control_pad() * 2 + chevron_w()
        }
        // A text entry that is not told to grow is the width of a search box:
        // room for a sentence, which is what one in a row is for.
        Tag::Field | Tag::Editor => sc(260),
        Tag::Spreadsheet => wanted_width(fonts, tree, index),
        _ => sc(160),
    }
}
/// The width of one character in a node's own style. A digit's, because a
/// spreadsheet column is mostly numbers and `0` is a fair average anyway.
pub(super) fn character_width(fonts: &Fonts, tree: &Tree, index: usize) -> i32 {
    fonts.measure("0", &style_at(tree, index)).max(1)
}

/// The narrowest a dragged column may be made.
pub fn column_floor(fonts: &Fonts, tree: &Tree, index: usize) -> i32 {
    character_width(fonts, tree, index) * MIN_COLUMN_CHARS
}

/// Break text into lines no wider than `width`, at spaces where possible.
///
/// Greedy, which is what every desktop does and what a reader expects: a line
/// takes as many words as fit. A single word wider than the line is broken
/// between characters rather than overflowing, because a URL or a long number
/// clipped at the edge is text the human cannot read at all. Newlines in the
/// text break lines too, so a message typed with returns keeps them.
pub fn wrap<'a>(fonts: &Fonts, text: &'a str, style: &Style, width: i32) -> Vec<&'a str> {
    fonts
        .break_lines(text, style, width)
        .iter()
        .map(|&(from, to)| &text[from as usize..to as usize])
        .collect()
}

/// The first `caret` characters of a string, as a slice.
pub(super) fn prefix(text: &str, caret: usize) -> &str {
    match text.char_indices().nth(caret) {
        Some((at, _)) => &text[..at],
        None => text,
    }
}

/// The selected run, low to high, or `None` when the caret is a point.
pub(super) fn selection(focus: &Focus) -> Option<(usize, usize)> {
    let anchor = focus.anchor?;
    let (from, to) = (anchor.min(focus.caret), anchor.max(focus.caret));
    (from != to).then_some((from, to))
}

/// Where a text control's content sits: the left edge it starts at, the top
/// of its first line, and how far apart its lines are.
#[derive(Clone, Copy)]
pub(super) struct TextBox {
    pub(super) left: i32,
    pub(super) top: i32,
    pub(super) step: i32,
}

/// Paint the highlight behind a selected run.
///
/// Behind, so the text stays the text: a selection that covered the words
/// would be a selection nobody could read. Walked a line at a time, because
/// an editor's run can cross newlines and each line starts again at the left.
pub(super) fn paint_selection(
    canvas: &mut Canvas,
    fonts: &Fonts,
    value: &str,
    style: &Style,
    box_: TextBox,
    (from, to): (usize, usize),
) {
    let TextBox { left, top, step } = box_;
    let mut at = 0;
    for (row, line) in value.split('\n').enumerate() {
        let length = line.chars().count();
        let start = at.max(from);
        let end = (at + length).min(to);
        if start < end {
            let x0 = left + fonts.measure(prefix(line, start - at), style);
            let x1 = left + fonts.measure(prefix(line, end - at), style);
            canvas.fill_rect(Rect::new(x0, top + row as i32 * step, (x1 - x0).max(1), step), selected());
        }
        // The newline the split consumed counts too, so a run that crosses
        // lines lands on the right characters of the next one.
        at += length + 1;
    }
}


