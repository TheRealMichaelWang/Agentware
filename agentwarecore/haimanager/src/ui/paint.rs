//! Painting: turning a laid-out document into pixels.
//!
//! Split from the layout that feeds it because they are two jobs with one
//! boundary between them, and because between them they were three thousand
//! lines in one file. Layout answers where everything is; this answers what
//! it looks like, reading the rectangles layout produced and never computing
//! one of its own.
//!
//! `use super::*` rather than a list of imports: this is the other half of
//! its parent, and the metrics, colours and helpers it draws with are the
//! parent's. Splitting the file was meant to make it readable, not to make
//! every shared helper public.

use super::*;

pub fn paint(
    canvas: &mut Canvas,
    fonts: &Fonts,
    content: &Content,
    tree: &Tree,
    layout: &Layout,
    focus: &Focus,
) {
    paint_subtree(canvas, fonts, content, tree, layout, Tree::ROOT, focus);
}

/// Paint one branch of a document.
///
/// A workspace's regions are separate branches of one tree that do not paint
/// consecutively: the wallpaper goes down, then the application windows on top
/// of it, then the side pane and the taskbar above those. Painting has to be
/// interruptible at the top level for that to be possible.
pub fn paint_subtree(
    canvas: &mut Canvas,
    fonts: &Fonts,
    content: &Content,
    tree: &Tree,
    layout: &Layout,
    index: usize,
    focus: &Focus,
) {
    paint_node(canvas, fonts, content, tree, layout, index, focus);

    // The floating options of an open dropdown are painted by a popup pass
    // after everything else, which for a whole document happens in the
    // window's own arm. A region is a branch painted *without* its window,
    // so that pass never runs for it, and the desk's dropdown was a control
    // that opened invisibly: hit-testable, painted nowhere. The same pass
    // runs here for any open select inside this branch; the root is left to
    // the window arm, or every open list would paint twice.
    if index != Tree::ROOT {
        for select in tree.open_overlays() {
            if tree.within(select, index) {
                paint_popup(canvas, fonts, content, tree, layout, select, focus);
            }
        }
    }
}

/// A thin indicator beside content that overflows.
///
/// Drawn after a scroll container's children so it sits above them, and only
/// when there is something out of sight: a bar on content that fits would say
/// something untrue.
/// The track and thumb of a scroll container's bar, or `None` when the content
/// fits and there is nothing to indicate.
///
/// One function, used by both painting and hit testing, so the thumb the hand
/// grabs is exactly the thumb the eye sees. Two copies of this arithmetic would
/// drift, and a scrollbar that moves under a click it does not answer to is the
/// kind of bug nobody files and everybody feels.
pub fn scrollbar_geometry(rect: Rect, scroller: &Scroller) -> Option<(Rect, Rect)> {
    if scroller.content <= scroller.viewport || scroller.viewport <= 0 {
        return None;
    }
    let furthest = scroller.furthest().max(1);

    if scroller.horizontal {
        let track = Rect::new(
            rect.x + sc(2),
            rect.y + rect.h - scrollbar_w() - sc(2),
            rect.w - sc(4),
            scrollbar_w(),
        );
        let span = (track.w * scroller.viewport / scroller.content).max(sc(24));
        let travel = track.w - span;
        let left = track.x + travel * scroller.offset / furthest;
        return Some((track, Rect::new(left, track.y, span, track.h)));
    }

    let track = Rect::new(
        rect.x + rect.w - scrollbar_w() - sc(2),
        rect.y + sc(2),
        scrollbar_w(),
        rect.h - sc(4),
    );
    let span = (track.h * scroller.viewport / scroller.content).max(sc(24));
    let travel = track.h - span;
    let top = track.y + travel * scroller.offset / furthest;
    Some((track, Rect::new(track.x, top, track.w, span)))
}

/// A thin indicator beside content that overflows.
///
/// Drawn after a scroll container's children so it sits above them, and only
/// while [`Focus::scrollbar`] says this container's content is being moved: the
/// bar hugs the content's edge, so earning its keep means leaving when the
/// scrolling stops.
pub(super) fn paint_scrollbar(canvas: &mut Canvas, layout: &Layout, index: usize, lit: Option<usize>) {
    for scroller in layout.scrollers.iter().filter(|s| s.node == index) {
        // A scroll container's bar appears while its content is moving and
        // leaves after. A grid's stays: how much sheet there is below the
        // screen is something a spreadsheet has to say all the time, and it
        // is the only thing saying it.
        if !scroller.permanent && !scroller.horizontal && lit != Some(index) {
            continue;
        }
        let Some((track, thumb)) = scrollbar_geometry(layout.rects[index], scroller) else {
            continue;
        };
        // No track drawn, only the thumb. A permanent groove down the side of
        // every scrollable thing is most of what makes a list look heavy.
        canvas.fill_round_rect(thumb, track.w.min(track.h) / 2, muted());
    }
}

/// An open dropdown's options: a raised panel hanging off the box, over
/// whatever it covers, painted after everything else in the window.
pub(super) fn paint_popup(
    canvas: &mut Canvas,
    fonts: &Fonts,
    content: &Content,
    tree: &Tree,
    layout: &Layout,
    select: usize,
    focus: &Focus,
) {
    let options = &tree.node(select).children;
    let Some(&first) = options.first() else { return };
    let Some(&last) = options.last() else { return };
    let top = layout.rects[first];
    let bottom = layout.rects[last];
    let panel = Rect::new(top.x, top.y, top.w, bottom.y + bottom.h - top.y);
    if panel.w <= 0 || panel.h <= 0 {
        return;
    }
    // The options' clip, not the box's: the panel floats with them, past
    // whatever container the box itself is confined to.
    let clip = layout.clips[first];
    canvas.clipped(clip, |canvas| {
        canvas.shadow(panel, radius_control(), sc(10), 120);
        canvas.fill_round_rect(panel, radius_control(), raised());
        canvas.stroke_round_rect(panel, radius_control(), 1, border());
    });
    for &option in options {
        paint_node(canvas, fonts, content, tree, layout, option, focus);
    }
}

/// The face every pressable surface in the system wears.
///
/// One function because the navigation bar's agentdesk tabs *are* buttons:
/// `nav_markup` writes them as `button` with `emphasis="primary"` on the
/// current one. A `tab` element that painted itself would be the same thing
/// drawn twice, and the two would drift the first time either was touched.
/// What differs between them is only what goes on top of the face: a label,
/// an icon, a glyph.
pub(super) fn paint_control_face(
    canvas: &mut Canvas,
    rect: Rect,
    emphasis: Option<&str>,
    disabled: bool,
    pressed: bool,
    focused: bool,
) {
    let fill = match (disabled, pressed, emphasis) {
        (true, _, _) => surface(),
        (_, true, Some("primary")) => accent_deep(),
        (_, true, Some("danger")) => danger_deep(),
        // `self::`, because callers have a local `pressed` shadowing the
        // palette function of the same name.
        (_, true, _) => self::pressed(),
        (_, _, Some("primary")) => accent(),
        (_, _, Some("danger")) => danger(),
        _ => raised(),
    };
    // Lit faintly from above, except when disabled (flat says inert) or
    // pressed (a control being pushed in should not look raised).
    if disabled || pressed {
        canvas.fill_round_rect(rect, radius_control(), fill);
    } else {
        canvas.fill_round_rect_vgrad(rect, radius_control(), lift(fill, 10), fill);
    }
    if focused && !disabled {
        canvas.stroke_round_rect(rect, radius_control(), 2, accent());
    } else if emphasis.is_none() {
        canvas.stroke_round_rect(rect, radius_control(), 1, border());
    }
}

/// The area a node's own painting can touch.
///
/// Its rectangle, with two exceptions: a strip of tabs paints its band the
/// full width of whatever it is clipped to rather than of its own box, and a
/// dialog carries a shadow that falls outside it. Getting this wrong does not
/// fail loudly, which is why it is one function: a node that paints outside
/// what this returns loses that part of itself at the edge of a partial
/// repaint, and a partial repaint is exactly the case nobody looks at.
pub(super) fn footprint(tag: Tag, rect: Rect, clip: Rect) -> Rect {
    match tag {
        Tag::Tabs => tabs_band(rect, clip),
        Tag::Dialog => rect.inset(-DIALOG_SHADOW),
        _ => rect,
    }
}

pub(super) fn paint_node(
    canvas: &mut Canvas,
    fonts: &Fonts,
    content: &Content,
    tree: &Tree,
    layout: &Layout,
    index: usize,
    focus: &Focus,
) {
    // Nothing in this branch can paint outside this node's clip: layout gives
    // a child its parent's clip or a piece of it, never more. So a clip the
    // canvas has already excluded is a whole branch that can be skipped
    // rather than walked, which is what makes a repaint of the region a
    // dragged window swept cost the window and not the desk.
    if layout.clips[index].intersect(&canvas.clip()).is_none() {
        return;
    }

    let node = tree.node(index);
    let rect = layout.rects[index];
    let disabled = node.disabled();
    let style = style_at(tree, index);
    let focused = focus.node == Some(index);
    let pressed = focus.pressed == Some(index);

    // A pressed control sinks by a pixel. Small enough not to reflow
    // anything, large enough that a still frame shows which control was just
    // acted on.
    //
    // A tab and a menu are exempt, and the exemption lives here rather than
    // in either click path. What they did is visible in what they became:
    // the tab is now the chosen one, the menu is now open. A flash on top of
    // that reads as a button being clicked, which is the one thing they must
    // not look like. The navigation bar's tabs have never flashed, because
    // its clicks never reach `Client::act`; expressing the rule in the paint
    // both paths share is what stops the two ever disagreeing again.
    let shows_press = !matches!(node.tag, Tag::Tab | Tag::Menu);
    let pressed = pressed && shows_press;
    let rect = if pressed { Rect::new(rect.x, rect.y + 1, rect.w, rect.h) } else { rect };

    // Disabled always wins over a colour the application chose: a control that
    // cannot be used must not look like one that can.
    let ink = if disabled { muted() } else { color_at(tree, index, text()) };

    // Text sits vertically centred in whatever box it was given.
    let centred = |height: i32| rect.y + (height - fonts.line_height(&style)) / 2;

    // Nothing draws outside the region its containers left it. The clip is
    // computed once during layout and reused here, so what is painted and what
    // is hit-testable cannot disagree.
    let clip = layout.clips[index];

    // A node whose own box is out of view paints nothing, and settling that
    // here rather than one primitive at a time is the difference between a
    // scrolled-away transcript line costing a rectangle test and costing a
    // line break plus a glyph lookup per character. Said as an empty clip
    // rather than as a branch, because every arm below clips and a clip
    // nothing intersects discards the lot at the top; the alternative is
    // another level of indentation around five hundred lines. Children are
    // still walked: a scroll container's content is taller than the
    // container, so a box out of view can hold something in view.
    let clip = match footprint(node.tag, rect, clip).intersect(&canvas.clip()) {
        Some(_) => clip,
        None => Rect::new(rect.x, rect.y, 0, 0),
    };

    canvas.clipped(clip, |canvas| match node.tag {
        Tag::Window => {
            canvas.fill_rect(rect, background());
            // The menu bar's band, drawn by the window rather than by the
            // menus in it, because a bar is a bar all the way across and not
            // only where a title happens to sit. A raised strip with a
            // hairline under it, which is what every other band on screen is.
            if has_menus(tree, index) {
                let band = menu_band(fonts, rect);
                canvas.fill_rect(band, surface());
                canvas.fill_rect(Rect::new(band.x, band.y + band.h - 1, band.w, 1), border());
            }
        }

        // Text sits in the vertical middle of whatever box it was given. In a
        // column the box is exactly its lines and this changes nothing; in a
        // row beside buttons the box is the row's height, and a label at the
        // top of it reads as misplaced next to controls whose text is centred.
        Tag::Text => {
            let words = label_of(node);
            // The run the human dragged out, behind the words, so they stay
            // readable. Static text is not a control and holds no caret; the
            // compositor tracks a run over it for one reason, which is that
            // an answer worth reading is an answer worth copying.
            if let Some((node, from, to)) = focus.run
                && node == index
            {
                paint_text_run(canvas, fonts, words, &style, rect, (from, to));
            }
            let lines = wrap(fonts, words, &style, rect.w);
            let (top, step) = text_rows(fonts, lines.len(), &style, rect);
            for (row, line) in lines.into_iter().enumerate() {
                canvas.draw_text(fonts, line, rect.x, top + row as i32 * step, &style, ink);
            }
        }

        Tag::Divider => canvas.fill_rect(rect, border()),

        // A picture, fitted to cover its rectangle. A source that cannot be
        // loaded leaves the words meant for an agent: the alt text, muted, so a
        // broken path is visible on screen rather than a silent hole.
        Tag::Image => match node.attr("src").and_then(|src| content.images.get(src, rect.w, rect.h)) {
            Some(bitmap) => canvas.blit(&bitmap, rect.x, rect.y),
            None => {
                canvas.fill_rect(rect, surface());
                canvas.draw_text(
                    fonts,
                    node.attr("alt").unwrap_or("?"),
                    rect.x + control_pad(),
                    rect.y + control_pad(),
                    &style,
                    muted(),
                );
            }
        },

        Tag::Icon => {
            canvas.draw_text(
                fonts,
                node.attr("alt").unwrap_or("?"),
                rect.x,
                centred(rect.h),
                &style,
                muted(),
            );
        }

        Tag::Dialog => {
            canvas.shadow(rect, radius_surface(), DIALOG_SHADOW, 120);
            canvas.fill_round_rect(rect, radius_surface(), raised());
            canvas.stroke_round_rect(rect, radius_surface(), 1, border());
            if let Some(label) = node.attr("label") {
                canvas.draw_text(fonts, label, rect.x + padding(), rect.y + padding(), &style, muted());
            }
        }

        // A group draws a hairline and a small label, and nothing else. It is a
        // heading with a rule under it, not a container: a filled box inside a
        // filled window inside a filled region is three surfaces deep and none
        // of them carries information.
        Tag::Group | Tag::List => {
            let heading = style_at(tree, index);
            let heading = Style { size: heading.size * 0.85, ..heading };
            if let Some(label) = node.attr("label") {
                canvas.draw_text(fonts, label, rect.x, rect.y, &heading, muted());
                let rule = rect.y + fonts.line_height(&heading) + 3;
                canvas.fill_rect(Rect::new(rect.x, rule, rect.w, 1), border());
            }
        }

        Tag::Button => {
            let emphasis = node.attr("emphasis");
            paint_control_face(canvas, rect, emphasis, disabled, pressed, focused);

            let label = label_of(node);
            let icon = node
                .attr("icon")
                .and_then(|name| content.images.icons.get(name, if node.flag("tile") { tile_icon() } else { button_icon() }));
            // A named glyph the compositor draws, for chrome-shaped buttons
            // whose meaning is a shape rather than a word: the pane's send
            // and stop. Compositor-internal like `width`; never an agent's
            // to see, because the description carries the meaning.
            if let Some(glyph) = node.attr("glyph") {
                draw_button_glyph(canvas, rect, glyph, ink);
            } else if node.flag("tile") {
                // Icon centred above the label. A missing icon leaves the
                // label where it is, so the grid does not jump.
                let top = rect.y + tile_pad();
                if let Some(icon) = icon {
                    canvas.blend_pixmap(icon.as_ref(), rect.x + (rect.w - tile_icon()) / 2, top);
                }
                let x = rect.x + (rect.w - fonts.measure(label, &style)) / 2;
                canvas.clipped(rect.inset(2), |canvas| {
                    canvas.draw_text(fonts, label, x, top + tile_icon() + sc(6), &style, ink);
                });
            } else {
                let icon_w = icon.as_ref().map(|_| button_icon() + sc(6)).unwrap_or(0);
                let width = fonts.measure(label, &style) + icon_w;
                let mut x = rect.x + (rect.w - width) / 2;
                if let Some(icon) = icon {
                    canvas.blend_pixmap(icon.as_ref(), x, rect.y + (rect.h - button_icon()) / 2);
                    x += icon_w;
                }
                canvas.draw_text(fonts, label, x, centred(rect.h), &style, ink);
            }
        }

        Tag::Field | Tag::Editor => {
            canvas.fill_round_rect(rect, radius_control(), background());
            let edge = match (focused, node.flag("invalid")) {
                (_, true) => danger(),
                (true, _) => accent(),
                _ => border(),
            };
            canvas.stroke_round_rect(rect, radius_control(), if focused { 2 } else { 1 }, edge);

            let value = value_of(node);
            let empty = value.is_empty();
            let content = if empty {
                node.attr("placeholder").unwrap_or("").to_owned()
            } else {
                value.clone()
            };
            let color = if empty { muted() } else { ink };

            // The content slides to keep the caret in view. Only while
            // focused: an unfocused control shows its start, and has no caret
            // to follow anyway.
            let (hshift, vshift) = if focused {
                text_scroll(fonts, &value, &style, node.tag, focus.caret, rect)
            } else {
                (0, 0)
            };
            let x = rect.x + control_pad() - hshift;
            let top =
                (if node.tag == Tag::Editor { rect.y + control_pad() } else { centred(rect.h) })
                    - vshift;
            let step = fonts.line_height(&style);

            canvas.clipped(rect.inset(1), |inner| {
                // The highlight goes down first, so the words sit on top of
                // it rather than under it.
                if focused && !empty && let Some(range) = selection(focus) {
                    paint_selection(inner, fonts, &value, &style, TextBox { left: x, top, step }, range);
                }

                // An editor is multi-line, so its value is drawn a line at a
                // time. A field is one line and any newline in it would be a
                // client bug, so it is drawn as it stands.
                if node.tag == Tag::Editor {
                    for (row, line) in content.lines().enumerate() {
                        inner.draw_text(fonts, line, x, top + row as i32 * step, &style, color);
                    }
                    if content.is_empty() {
                        inner.draw_text(fonts, &content, x, top, &style, color);
                    }
                } else {
                    inner.draw_text(fonts, &content, x, top, &style, color);
                }

                if !focused || !focus.caret_visible {
                    return;
                }

                // The caret is the compositor's, not the application's. It is
                // placed against the value rather than against whatever
                // placeholder is standing in for it.
                let (row, column) = caret_position(&value, focus.caret, node.tag);
                let line = value.lines().nth(row).unwrap_or("");
                let caret_x = x + fonts.measure(prefix(line, column), &style);
                inner.fill_rect(Rect::new(caret_x, top + row as i32 * step, sc(2).max(2), step), accent());
            });
        }

        Tag::Slider => {
            let range = crate::slider::Range::of(node);
            let value = crate::slider::value_of(node, range);
            crate::slider::draw(canvas, rect, value, range, focused, disabled);
        }

        Tag::Checkbox => {
            let box_rect = Rect::new(
                rect.x,
                rect.y + (rect.h - checkbox_size()) / 2,
                checkbox_size(),
                checkbox_size(),
            );
            let checked = node.flag("checked");
            canvas.fill_round_rect(
                box_rect,
                radius_small(),
                if checked && !disabled { accent() } else { background() },
            );
            if !checked || disabled {
                canvas.stroke_round_rect(box_rect, radius_small(), 1, if focused { accent() } else { border() });
            }
            if checked && disabled {
                canvas.fill_round_rect(box_rect.inset(4), 2, muted());
            } else if checked {
                // A tick rather than a filled square: a square inside a square
                // reads as a loading state. Drawn as two strokes stepped along
                // the box's own geometry, because the previous version was a
                // hand-tuned cluster of rectangles that stopped lining up the
                // first time the box changed size.
                let t = box_rect.inset(3);
                let (w, h) = (t.w as f32, t.h as f32);
                let low = (t.x as f32 + w * 0.36, t.y as f32 + h * 0.72);
                let strokes = [
                    ((t.x as f32 + w * 0.08, t.y as f32 + h * 0.46), low),
                    (low, (t.x as f32 + w * 0.90, t.y as f32 + h * 0.12)),
                ];
                for ((ax, ay), (bx, by)) in strokes {
                    canvas.stroke_line(ax, ay, bx, by, sc(2).max(2), background());
                }
            }
            canvas.draw_text(
                fonts,
                label_of(node),
                rect.x + checkbox_size() + 8,
                centred(rect.h),
                &style,
                ink,
            );
        }

        // The dropdown as it sits closed or open: a box like a field with the
        // chosen option's words and a chevron. Its options paint separately,
        // last, so they float over what follows.
        Tag::Select => {
            let open = node.flag("open");
            canvas.fill_round_rect(rect, radius_control(), background());
            let edge = if focused || open { accent() } else { border() };
            canvas.stroke_round_rect(rect, radius_control(), if focused || open { 2 } else { 1 }, edge);
            let chosen = node.attr("value").unwrap_or("");
            let shown = node
                .children
                .iter()
                .map(|&child| tree.node(child))
                .find(|option| option.attr("value").unwrap_or(label_of(option)) == chosen)
                .map(label_of)
                .filter(|label| !label.is_empty());
            let (words, color) = match shown {
                Some(label) => (label.to_owned(), ink),
                None => (node.attr("placeholder").unwrap_or("").to_owned(), muted()),
            };
            canvas.clipped(Rect::new(rect.x, rect.y, rect.w - chevron_w(), rect.h), |canvas| {
                canvas.draw_text(fonts, &words, rect.x + control_pad(), centred(rect.h), &style, color);
            });
            // The chevron: down while closed, up while open.
            let cx = (rect.x + rect.w - chevron_w() / 2) as f32;
            let cy = (rect.y + rect.h / 2) as f32;
            let r = sc(3) as f32;
            let t = sc(1).max(1);
            let dy = if open { -r * 0.7 } else { r * 0.7 };
            canvas.stroke_line(cx - r, cy - dy * 0.5, cx, cy + dy * 0.5, t, muted());
            canvas.stroke_line(cx, cy + dy * 0.5, cx + r, cy - dy * 0.5, t, muted());
        }

        Tag::Option => {
            if node.flag("selected") {
                canvas.fill_rect(rect, selected());
            }
            if focused {
                canvas.stroke_rect(rect, 1, accent());
            }
            canvas.draw_text(fonts, label_of(node), rect.x + control_pad(), centred(rect.h), &style, ink);
        }

        Tag::Item => {
            if node.flag("selected") {
                canvas.fill_rect(rect, selected());
            }
            if focused {
                canvas.stroke_rect(rect, 1, accent());
            }
            canvas.draw_text(
                fonts,
                label_of(node),
                rect.x + control_pad(),
                centred(rect.h),
                &style,
                ink,
            );
        }

        // The grid itself: the surface under it, the frozen gutter down its
        // left, and the row labels in that gutter. The gutter is painted here
        // rather than by each row because it is one strip that happens to
        // hold one label per row, and because a row does not know how wide it
        // is: that is a property of the table's widest label.
        // A grid, drawn from the sheet the element points at. Nothing here
        // is a node: the geometry comes from the one `Grid` layout produced
        // and the values from the stream the application published, so a
        // screenful costs a screenful whatever the sheet is.
        Tag::Spreadsheet => {
            let Some(grid) = layout.grids.iter().find(|grid| grid.node == index) else {
                return;
            };
            let sheet = content.sheets.get(&grid.source);
            let cursor = node.attr("cursor").and_then(sheet::parse);
            let run = node.attr("selection").and_then(sheet::parse_range);
            // What is being typed wins over what was published: the local copy
            // is the one the human is changing, and it becomes the published
            // one when the application echoes it back.
            let editing = focus
                .cell
                .as_ref()
                .filter(|(at, _)| focused && Some(*at) == cursor)
                .map(|(_, value)| value.as_str());
            let ((first_column, first_row), (last_column, last_row)) = grid.visible();
            let numbers = Style { size: style.size * 0.9, ..style };

            canvas.fill_rect(rect, background());

            // The cells, then the lines between them, then what is chosen on
            // top: a run painted behind the words the way a text selection is.
            canvas.clipped(grid.body.intersect(&clip).unwrap_or(grid.body), |canvas| {
                for row in first_row..=last_row {
                    for column in first_column..=last_column {
                        let at = (column, row);
                        let box_ = grid.cell_rect(at);
                        let chosen = run.is_some_and(|((x0, y0), (x1, y1))| {
                            (x0..=x1).contains(&column) && (y0..=y1).contains(&row)
                        });
                        if chosen {
                            canvas.fill_rect(box_, selected());
                        }
                        canvas.fill_rect(
                            Rect::new(box_.x + box_.w - 1, box_.y, 1, box_.h),
                            border(),
                        );
                        canvas.fill_rect(
                            Rect::new(box_.x, box_.y + box_.h - 1, box_.w, 1),
                            border(),
                        );
                        // The cell being typed into is drawn last, over its
                        // neighbours and as wide as its contents.
                        if editing.is_some() && Some(at) == cursor {
                            continue;
                        }
                        let Some(value) = sheet.and_then(|s| s.get(at)) else {
                            continue;
                        };
                        canvas.clipped(box_.inset(1), |cell| {
                            cell.draw_text(
                                fonts,
                                value,
                                box_.x + control_pad(),
                                box_.y + (box_.h - fonts.line_height(&style)) / 2,
                                &style,
                                ink,
                            );
                        });
                    }
                }

                // The cell being typed into, over everything: a box as wide as
                // what is in it, so a value longer than its column can be read
                // and edited rather than trimmed at the column's edge. What
                // every spreadsheet does, and the reason it can be done here
                // is that the box is the compositor's own copy rather than a
                // node with a rectangle somebody else decided.
                if let (Some(value), Some(at)) = (editing, cursor) {
                    let box_ = grid.cell_rect(at);
                    let step = fonts.line_height(&style);
                    let wanted = fonts.measure(value, &style) + control_pad() * 2 + sc(4);
                    let box_ = Rect::new(
                        box_.x,
                        box_.y,
                        box_.w.max(wanted).min((grid.body.x + grid.body.w - box_.x).max(box_.w)),
                        box_.h,
                    );
                    let left = box_.x + control_pad();
                    let top = box_.y + (box_.h - step) / 2;
                    canvas.fill_rect(box_, background());
                    canvas.clipped(box_.inset(1), |cell| {
                        if let Some(range) = selection(focus) {
                            paint_selection(
                                cell,
                                fonts,
                                value,
                                &style,
                                TextBox { left, top, step },
                                range,
                            );
                        }
                        cell.draw_text(fonts, value, left, top, &style, ink);
                        if focus.caret_visible {
                            let column = focus.caret.min(value.chars().count());
                            let caret = left + fonts.measure(prefix(value, column), &style);
                            cell.fill_rect(Rect::new(caret, top, sc(2).max(2), step), accent());
                        }
                    });
                    canvas.stroke_rect(box_, sc(2).max(2), accent());
                } else if let Some(at) = cursor {
                    // The cursor when nothing is being typed: a ring rather
                    // than a fill, so which cell is current and which cells
                    // are chosen stay two visibly different things.
                    canvas.stroke_rect(grid.cell_rect(at), sc(2).max(2), accent());
                }
            });

            // The column letters. Frozen: they do not scroll down, and the
            // cursor's column is lit so a wide sheet still says where you are.
            canvas.clipped(grid.header.intersect(&clip).unwrap_or(grid.header), |canvas| {
                canvas.fill_round_rect_vgrad(grid.header, 0, lift(raised(), 6), raised());
                for column in first_column..=last_column {
                    let box_ = Rect::new(
                        grid.body.x + grid.column_x(column) - grid.offset.0,
                        grid.header.y,
                        grid.column_w(column),
                        grid.header.h,
                    );
                    if cursor.is_some_and(|(at, _)| at == column) {
                        canvas.fill_rect(box_, selected());
                    }
                    canvas.fill_rect(Rect::new(box_.x + box_.w - 1, box_.y, 1, box_.h), border());
                    let label = sheet::column_name(column);
                    let width = fonts.measure(&label, &style);
                    canvas.draw_text(
                        fonts,
                        &label,
                        box_.x + ((box_.w - width) / 2).max(control_pad()),
                        grid.header.y + (grid.header.h - fonts.line_height(&style)) / 2,
                        &style,
                        text(),
                    );
                }
            });

            // The row numbers, frozen the other way.
            canvas.clipped(grid.gutter.intersect(&clip).unwrap_or(grid.gutter), |canvas| {
                canvas.fill_round_rect_vgrad(grid.gutter, 0, lift(surface(), 4), surface());
                for row in first_row..=last_row {
                    let top = grid.body.y + row as i32 * grid.row_h - grid.offset.1;
                    if cursor.is_some_and(|(_, at)| at == row) {
                        canvas.fill_rect(
                            Rect::new(grid.gutter.x, top, grid.gutter.w, grid.row_h),
                            selected(),
                        );
                    }
                    let label = (row + 1).to_string();
                    let width = fonts.measure(&label, &numbers);
                    canvas.draw_text(
                        fonts,
                        &label,
                        grid.gutter.x + (grid.gutter.w - width) / 2,
                        top + (grid.row_h - fonts.line_height(&numbers)) / 2,
                        &numbers,
                        muted(),
                    );
                }
            });

            // The corner where the two frozen strips meet, and the edges.
            canvas.fill_round_rect_vgrad(
                Rect::new(rect.x, rect.y, grid.gutter.w, grid.header.h),
                0,
                lift(raised(), 6),
                raised(),
            );
            canvas.fill_rect(
                Rect::new(rect.x + grid.gutter.w - 1, rect.y, 1, rect.h),
                border(),
            );
            canvas.fill_rect(
                Rect::new(rect.x, rect.y + grid.header.h - 1, rect.w, 1),
                border(),
            );
            canvas.stroke_round_rect(rect, radius_small(), 1, border());
        }

        // A menu: a label you press, or nothing at all. An unlabelled one is
        // a place for its items to hang from, opened by whatever the
        // application decided a right-click means.
        Tag::Menu => {
            let label = label_of(node);
            if label.is_empty() {
                // Nothing to draw: it exists to be named and to hang its
                // items from.
            } else {
                // Words on the band, lit only while its list is showing. The
                // fill used to follow focus as well, so pressing it once left
                // a filled box standing behind the word for as long as the
                // keyboard pointed there, and a menu wearing a box is a
                // button. Focus is a hairline instead: visible to whoever is
                // tabbing through and to nobody else.
                if node.flag("open") {
                    canvas.fill_round_rect(rect, radius_control(), theme().pressed);
                } else if focused && !disabled {
                    canvas.stroke_round_rect(rect, radius_control(), 1, accent());
                }
                let width = fonts.measure(label, &style);
                canvas.draw_text(
                    fonts,
                    label,
                    rect.x + ((rect.w - width) / 2).max(button_pad()),
                    centred(rect.h),
                    &style,
                    ink,
                );
            }
        }

        Tag::MenuItem => {
            if focused {
                canvas.fill_rect(rect, theme().pressed);
            }
            canvas.draw_text(
                fonts,
                label_of(node),
                rect.x + control_pad(),
                centred(rect.h),
                &style,
                ink,
            );
        }

        // The band, which is the whole of why the navigation bar's tabs look
        // the way they do.
        //
        // Its tabs are unemphasised buttons on a `raised` strip, and an
        // unemphasised button fills with `raised`: the fill disappears into
        // the band and what is left is text. The plus at its end disappears
        // the same way. So a tab strip paints the band first and everything
        // in it comes out looking like the navigation bar without being told
        // to, including whatever an application puts in the strip beside its
        // tabs.
        Tag::Tabs => {
            // The navigation bar's structure, reproduced: a thin raised cap,
            // the tabs standing on the same colour as the desk behind, a
            // thin raised foot, and the hairline under it. The tabs are not
            // on the raised part; that is the whole difference between a tab
            // cut into a bar and a button lying on one.
            //
            // It reaches the document's edges rather than the strip's own
            // rectangle, because the bar the screen paints under the
            // navigation document does, and one that stopped short of the
            // window's sides would be a floating row.
            let band = tabs_band(rect, clip);
            let line = band.y + band.h - 1;
            // Raised everywhere, then the desk's own colour punched into the
            // middle where the tabs stand. What is left raised is a cap
            // above, a foot below, and a margin at each end: exactly what the
            // navigation bar was, where the screen filled the bar and the
            // document's window painted its inset frame over it. The punched
            // rectangle is the same frame the tabs were laid out in, from
            // the same function, so the two cannot part company.
            canvas.fill_rect(band, raised());
            canvas.fill_rect(tabs_frame(fonts, rect, clip), background());
            canvas.fill_rect(Rect::new(band.x, line, band.w, 1), border());
        }

        // A tab is a button that says which one you are on, exactly as the
        // navigation bar's agentdesks are: the chosen one wears the primary
        // emphasis, the rest wear none and vanish into the band behind them.
        // Through the same face, so the two cannot drift apart.
        Tag::Tab => {
            let chosen = node.flag("selected");
            // Neither the press nor the focus ring is drawn. Both are states
            // the navigation bar's tabs never show, and the ring is what
            // makes switching quickly flash: the moment a tab is pressed it
            // takes focus and outlines itself in the accent, and only a
            // frame later, once the application has answered, does it fill.
            // An outline that becomes a fill is a flash. Being the chosen
            // one is the only thing a tab says about itself.
            paint_control_face(canvas, rect, chosen.then_some("primary"), disabled, false, false);

            // The words sit where they sat when the room for the cross was
            // four spaces on the end of them: the padded string is centred
            // and only the label itself is drawn.
            let label = label_of(node);
            let room = tab_label_room(label, node.flag("closable"));
            let width = fonts.measure(&room, &style);
            canvas.clipped(rect.inset(1), |canvas| {
                canvas.draw_text(
                    fonts,
                    label,
                    rect.x + ((rect.w - width) / 2).max(control_pad()),
                    centred(rect.h),
                    &style,
                    ink,
                );
            });
            if node.flag("closable") {
                draw_tab_close(canvas, rect, chosen);
            }
        }

        // Pure arrangement draws nothing at all.
        Tag::VStack | Tag::HStack | Tag::Scroll => {}
    });

    // Children in order, except that a window's dialogs go last, over the
    // content they float above, with the content dimmed under them so what is
    // inert looks inert; and open dropdowns' options go after even those.
    let node = tree.node(index);
    if node.tag == Tag::Window {
        for &child in &node.children {
            if tree.node(child).tag != Tag::Dialog {
                paint_node(canvas, fonts, content, tree, layout, child, focus);
            }
        }
        let dialogs: Vec<usize> =
            node.children.iter().copied().filter(|&c| tree.node(c).tag == Tag::Dialog).collect();
        if !dialogs.is_empty() {
            canvas.clipped(clip, |canvas| canvas.dim(rect, 96));
        }
        for child in dialogs {
            paint_node(canvas, fonts, content, tree, layout, child, focus);
        }
        for select in tree.open_overlays() {
            paint_popup(canvas, fonts, content, tree, layout, select, focus);
        }
    } else if node.tag == Tag::Select {
        // Options are painted by the popup pass, not here.
    } else {
        for &child in &node.children {
            paint_node(canvas, fonts, content, tree, layout, child, focus);
        }
    }

    if matches!(node.tag, Tag::Scroll | Tag::Spreadsheet) {
        canvas.clipped(clip, |canvas| paint_scrollbar(canvas, layout, index, focus.scrollbar));
    }
}

/// The shapes a button's `glyph` names, drawn as strokes and fills the way
/// the window controls are, because the shipped font cannot be trusted to
/// carry a paper plane. Cheap by construction: a handful of lines per press,
/// nothing rasterized per frame.
pub(super) fn draw_button_glyph(canvas: &mut Canvas, rect: Rect, glyph: &str, color: Color) {
    let size = sc(12);
    let cx = rect.x + rect.w / 2;
    let cy = rect.y + rect.h / 2;
    match glyph {
        // A paper plane: a dart pointing right, with a folded tail.
        "send" => {
            let l = (cx - size / 2) as f32;
            let r = (cx + size / 2) as f32;
            let t = (cy - size / 2) as f32;
            let b = (cy + size / 2) as f32;
            let mid = cy as f32;
            let tail = l + size as f32 * 0.3;
            let th = sc(2).max(2);
            canvas.stroke_line(l, t, r, mid, th, color);
            canvas.stroke_line(l, b, r, mid, th, color);
            canvas.stroke_line(l, t, tail, mid, th, color);
            canvas.stroke_line(l, b, tail, mid, th, color);
        }
        // The square that means stop, everywhere it means anything.
        "stop" => {
            let side = size - sc(2);
            canvas.fill_round_rect(
                Rect::new(cx - side / 2, cy - side / 2, side, side),
                radius_small(),
                color,
            );
        }
        // A glyph nobody has drawn yet: a hollow box says so on screen
        // rather than a silently blank button.
        _ => canvas.stroke_rect(Rect::new(cx - size / 2, cy - size / 2, size, size), 1, color),
    }
}

/// Turn a caret offset in characters into a line and a column.
///
/// A field has one line by construction, so it is column-only. An editor may
/// have many, and the caret has to survive a newline in the middle of the value.
pub fn caret_position(value: &str, caret: usize, tag: Tag) -> (usize, usize) {
    if tag != Tag::Editor {
        return (0, caret.min(value.chars().count()));
    }

    let mut row = 0;
    let mut column = 0;
    for (at, character) in value.chars().enumerate() {
        if at >= caret {
            break;
        }
        if character == '\n' {
            row += 1;
            column = 0;
        } else {
            column += 1;
        }
    }
    (row, column)
}

/// Which character of a text control's value a click at `x` landed on.
///
/// Measuring one prefix at a time rather than doing anything cleverer: a field
/// holds a line of text, this runs once per click, and a binary search over a
/// proportional font is not obviously correct at the ends.
pub fn caret_from_x(fonts: &Fonts, value: &str, style: &Style, left: i32, x: i32) -> usize {
    let mut best = 0;
    let mut closest = i32::MAX;
    for caret in 0..=value.chars().count() {
        let at = left + fonts.measure(prefix(value, caret), style);
        let distance = (at - x).abs();
        if distance < closest {
            closest = distance;
            best = caret;
        }
    }
    best
}

/// How far a text control's content is shifted so its caret stays in view:
/// left by the first number, up by the second.
///
/// A value longer than its box used to draw from the start regardless, so the
/// caret walked off the right edge and vanished, and typing appended to text
/// nobody could see. The content slides instead, exactly as far as the caret
/// needs and no further, so a caret at the start shows the start and a caret
/// past the edge drags the text along. Derived from the caret rather than
/// stored, which is what keeps the painter and the click hit test agreeing:
/// both call this, so where text is drawn and where a click lands cannot
/// drift. The vertical half is the same idea for an editor's lines.
pub fn text_scroll(
    fonts: &Fonts,
    value: &str,
    style: &Style,
    tag: Tag,
    caret: usize,
    rect: Rect,
) -> (i32, i32) {
    let (row, column) = caret_position(value, caret, tag);
    let line = value.split('\n').nth(row).unwrap_or("");
    let ahead = fonts.measure(prefix(line, column), style);
    // A margin keeps the caret a step inside the edge, so the next character
    // has somewhere visible to go.
    let inner_w = (rect.w - control_pad() * 2).max(sc(20));
    let hshift = (ahead - inner_w + sc(6)).max(0);

    let vshift = if tag == Tag::Editor {
        let step = fonts.line_height(style).max(1);
        let inner_h = (rect.h - control_pad() * 2).max(step);
        ((row as i32 + 1) * step - inner_h).max(0)
    } else {
        0
    };
    (hshift, vshift)
}

/// Which character of a text control's value a click landed on, row and all.
///
/// The shift is computed from the caret as it stands, because that is the
/// shift the content was painted with: a click lands on what the human sees.
/// Where a text element's wrapped lines are drawn: the top of the first one
/// and the step between them.
///
/// One function, because a run the human drags out has to land on the
/// characters they saw, which means the hit test and the paint have to agree
/// about where those characters are.
pub(super) fn text_rows(fonts: &Fonts, lines: usize, style: &Style, rect: Rect) -> (i32, i32) {
    let step = fonts.line_height(style);
    let block = lines as i32 * step;
    (rect.y + ((rect.h - block) / 2).max(0), step)
}

/// Which character of a wrapped text element a point lands on.
///
/// The wrapped lines come from the same cache the layout and the paint use,
/// so this costs a lookup rather than breaking the text again.
pub fn text_offset_at(
    fonts: &Fonts,
    text: &str,
    style: &Style,
    rect: Rect,
    at: (i32, i32),
) -> usize {
    let lines = fonts.break_lines(text, style, rect.w);
    if lines.is_empty() {
        return 0;
    }
    let (top, step) = text_rows(fonts, lines.len(), style, rect);
    let row = (((at.1 - top) / step.max(1)).max(0) as usize).min(lines.len() - 1);
    let (from, to) = lines[row];
    let line = &text[from as usize..to as usize];
    // The offset is counted in characters from the start of the whole string,
    // so what is before this line counts too.
    let before = text[..from as usize].chars().count();
    before + caret_from_x(fonts, line, style, rect.x, at.0)
}

/// Paint the highlight behind a run of *wrapped* text.
///
/// The twin of [`paint_selection`], which walks the newlines an editor's value
/// carries. This one walks the lines the compositor chose when it broke the
/// text to the width it was given, because static text has no newlines of its
/// own to walk.
pub(super) fn paint_text_run(
    canvas: &mut Canvas,
    fonts: &Fonts,
    text: &str,
    style: &Style,
    rect: Rect,
    (from, to): (usize, usize),
) {
    let lines = fonts.break_lines(text, style, rect.w);
    let (top, step) = text_rows(fonts, lines.len(), style, rect);
    for (row, &(start, end)) in lines.iter().enumerate() {
        let line = &text[start as usize..end as usize];
        let before = text[..start as usize].chars().count();
        let length = line.chars().count();
        let head = from.max(before);
        let tail = to.min(before + length);
        if head >= tail {
            continue;
        }
        let x0 = rect.x + fonts.measure(prefix(line, head - before), style);
        let x1 = rect.x + fonts.measure(prefix(line, tail - before), style);
        canvas.fill_rect(
            Rect::new(x0, top + row as i32 * step, (x1 - x0).max(1), step),
            selected(),
        );
    }
}

pub fn caret_at_point(
    fonts: &Fonts,
    value: &str,
    style: &Style,
    tag: Tag,
    rect: Rect,
    caret_now: usize,
    at: (i32, i32),
) -> usize {
    let (x, y) = at;
    let (hshift, vshift) = text_scroll(fonts, value, style, tag, caret_now, rect);
    let left = rect.x + control_pad() - hshift;
    if tag != Tag::Editor {
        return caret_from_x(fonts, value, style, left, x);
    }

    let step = fonts.line_height(style).max(1);
    let top = rect.y + control_pad() - vshift;
    let lines: Vec<&str> = value.split('\n').collect();
    let row = (((y - top) / step).max(0) as usize).min(lines.len().saturating_sub(1));
    let column = caret_from_x(fonts, lines[row], style, left, x);
    lines[..row].iter().map(|line| line.chars().count() + 1).sum::<usize>() + column
}

