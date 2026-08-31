# AWML: Element Catalogue

The Agentware Markup Language. This is the complete set of elements an application may emit, the state each carries, and the actions each accepts.

The governing rule is that **markup describes affordances, not arrangement**. An app never says "a rectangle 120x40 at these coordinates". It says "a button, id `send`, enabled, which sends the composed message". The haimanager decides where it goes and how big it is.

Applications *do* choose type and colour. That is safe rather than a compromise: appearance can never carry meaning here, because every control is required to have a description and its actions are derived from its type and state. An agent is never asked to infer anything from how something looks, so styling can be as expressive as an app likes.

The consequence is the reason for the whole design: the human's rendered view and the agent's semantic view are generated from *the same tree*. There is no derivation step between them and therefore no way for them to drift. Every other system builds a render tree and derives an accessibility tree from it, which is why accessibility trees are perpetually stale and wrong.

## Three Classes of Element

Not every element needs an identity. Forcing one contract on all of them would mean apps writing meaningless descriptions for layout scaffolding, and agents reading a tree that is mostly noise.

| Class | Needs an id | In the agent's view | Accepts actions |
| --- | --- | --- | --- |
| **Layout** | no | no, flattened away | no |
| **Content** | no | yes, for its text | no |
| **Control** | **yes, required** | yes | yes |

Layout elements exist only to arrange things on a screen. An agent does not need to know two buttons sit in a row; it needs to know both exist, what they do, and whether they are enabled. They are stripped from the agent's view and their children flatten upward into the parent.

## The Control Contract

Every control carries these. The first three are written by the app; the last two are computed by the haimanager.

| Field | Written by | Meaning |
| --- | --- | --- |
| `id` | app | Unique within its window. What an agent names to act on it. |
| `description` | app | What this control *does*, in a sentence. |
| `must-ask-perms` | app | The human must approve before an agent may act on it. Humans are never blocked. |
| `state` | app | `disabled`, `busy`, and type-specific state like `checked` or `value`. |
| `actions` | **haimanager** | Which actions are currently available. Derived, never declared. |

### Why actions are derived rather than declared

If an app wrote its own action list it could omit `focus`, invent an action nothing implements, or advertise `click` on a disabled button. The action space would stop being closed, and the agent would be reasoning about an open-ended vocabulary again — which is precisely what a fixed element set exists to prevent.

Instead the haimanager computes the list from the element's type and its current state. A `button` offers `focus` and `click`; a `disabled` button offers nothing. The agent still receives an explicit list, so nothing is left implicit for the model to infer. It is simply derived rather than trusted.

### Writing a good description

The description is the only thing telling an agent what a control means. Two rules:

* Describe the **effect**, not the appearance. "Sends the composed message" beats "the blue button".
* It is not the visible label. A label is for humans and is often absent — an icon-only button has none, which is exactly when an agent needs the description most.

`description` is required on every control. Optional documentation is documentation that does not exist.

### `must-ask-perms`

Declares that an agent may not act on this control without explicit human approval. It never restricts the human. Intended for irreversible or outward-facing actions: sending, deleting, purchasing, publishing.

It is left `false` throughout for now; the enforcement path is not built.

One thing worth not painting into a corner: an app declaring its own permissions is a starting point, not a security model. An app can mark a destructive action `false`, whether through carelessness or design. The human and the OS should eventually be able to raise a requirement the app did not ask for. Nothing here should assume the app's declaration is the final word.

## Styling

Five attributes, all of which **inherit**, so a window sets them once and any element can override:

| Attribute | Values |
| --- | --- |
| `font` | `sans`, `mono` |
| `size` | a number of pixels, or `xs` `sm` `md` `lg` `xl` |
| `weight` | `normal`, `bold` |
| `italic` | present or absent |
| `color` | `text` `muted` `accent` `danger` `ok`, or `#rrggbb` / `#rgb` |

Type is rendered from outline fonts, so a size is a real size rather than a multiple of a bitmap cell. `role` still sets sensible defaults: a `heading` is large and bold without being told to be.

**None of this reaches an agent.** Every styling attribute is dropped from the agent's view, along with `gap` and `placeholder`. The agent sees what a control is, what it says, what state it is in, and what it can do.

One thing styling cannot override: `disabled` always renders muted. A control that cannot be used must never look like one that can, and that is not the application's call to make.

## Layout Elements

No id, no description, no actions. Invisible to agents.

| Element | Attributes | Notes |
| --- | --- | --- |
| `vstack` | `gap`, `align`, `grow` | Stacks children top to bottom. |
| `hstack` | `gap`, `align`, `grow` | Stacks children left to right. |
| `grid` | `cols`, `gap` | Uniform columns. |
| `scroll` | `dir=x\|y\|both`, `anchor=end` | Clips and scrolls its overflow. Needs no id: an agent never addresses one, because acting on a node inside it scrolls it. `anchor="end"` keeps the end in view as content grows, until the human scrolls away from it: a transcript's behaviour. |
| `split` | `dir`, `ratio` | Two resizable panes. Used for the workspace side pane. |

There is no absolute positioning, no `z-index`, and no stylesheet. The layout model is deliberately small enough to implement deterministically and to reason about without simulation.

### The one container that survives

Flattening layout away is right for arrangement, but arrangement is sometimes the only thing carrying meaning. A settings page with a Billing section and a Shipping section, each holding Street, City and Postcode, flattens into six fields with nothing saying which belongs to which. The human sees two headed boxes; the agent sees an undifferentiated list.

`group` exists for exactly that. It is a container that is **semantic rather than visual**, so it is not stripped.

| Element | Attributes | Notes |
| --- | --- | --- |
| `group` | `label`, optional `id` | Survives into the agent's view with its children nested beneath it. |

Unique ids and good descriptions could paper over the problem (`billing-street`, "Street address for billing"), but that makes every app author redundantly encode structure into every description, and it throws away something a model can genuinely use: knowing there are two addresses to fill in rather than six fields to puzzle over.

The rule is the distinction, not the element: **strip what arranges, keep what means**. If a container exists to put things side by side, it goes. If it exists to say these things belong together, it stays.

`id` is optional on a group, since agents do not act on one. Give it an id only when something may need to bring it into view.

## Content Elements

Visible to agents, since an agent must be able to read a screen. They carry no id and accept no actions.

| Element | Attributes | Notes |
| --- | --- | --- |
| `text` | `role=heading\|subheading\|body\|caption\|label` | Role is semantic, not a font size. Wraps to the width it is given. |
| `icon` | `name`, `alt` | `alt` is what the agent reads. |
| `image` | `src`, `alt`, `fit=cover` | `alt` required. An agent that cannot read a picture must still know what it shows. `src` names a file (SVG or PNG) the haimanager loads and fits; pixels never cross the protocol. Placed among controls it takes a preview-sized 16:9 slot; placed as a region's whole content, it takes the region. |
| `divider` | `dir` | Purely visual, but cheap to keep. A hairline across a column, or down a row with `dir="vertical"`. |
| `progress` | `value` or `indeterminate`, `label` | State an agent needs: is something still running? |

## Controls

Every one requires `id` and `description`. The **Actions** column is what the haimanager derives when the control is enabled; a `disabled` control offers none.

### Buttons and links

| Element | Attributes | State | Actions |
| --- | --- | --- | --- |
| `button` | `label`, `icon`, `emphasis=normal\|primary\|danger` | `disabled`, `busy` | `focus` `click` |
| `link` | `label`, `to` | `disabled` | `focus` `click` |

`link` is separate from `button` because the distinction is semantic and agents act on it: a link navigates somewhere, a button performs an action. `to` describes the destination for the agent.

### Text entry

| Element | Attributes | State | Actions |
| --- | --- | --- | --- |
| `field` | `label`, `placeholder`, `kind=text\|password\|number\|search` | `value`, `disabled`, `invalid`, `error` | `focus` `type-text` `clear` `submit` |
| `editor` | `label`, `placeholder` | `value`, `disabled`, `invalid` | `focus` `type-text` `clear` |

`field` is one line and offers `submit`, which is the Enter key. `editor` is multi-line, where Enter inserts a newline, so it has no `submit`.

A `password` field reports `value` as a masked placeholder in the agent's view, never the contents.

### Choices

| Element | Attributes | State | Actions |
| --- | --- | --- | --- |
| `checkbox` | `label` | `checked`, `disabled` | `focus` `check` `uncheck` `toggle` |
| `radiogroup` | `label` | `disabled` | — |
| `radio` | `label` | `checked`, `disabled` | `focus` `select` |
| `select` | `label`, `placeholder` | `value`, `disabled`, `open` | `focus` `open` `close` |
| `option` | `label`, `value` | `selected`, `disabled` | `select` |

A `select` is a dropdown. Its `open` is the application's, like every other piece of state in its tree: the compositor asks with `open` and `close` events (a human pressing the box, or an agent's intent; both are unconditional, so one that is already that way sends nothing) and the application answers by re-rendering with `open="true"` and its `option` children, which then float below the box over whatever follows, painted after everything else in the window and hit-tested first. Pressing an option always reports `select` on it, even the one already chosen, because choosing is also what closes the list. A press anywhere else while it is open closes it and does nothing more. Closed, the options are nowhere: no rectangle, no actions, and `folded` from the compositor's point of view, so an agent's view lists them but offers no action until the list is open. Options carry ids, since an agent chooses one by naming it.
| `slider` | `label`, `min`, `max`, `step` | `value`, `disabled` | `focus` `set-value` |

`check` and `uncheck` exist alongside `toggle` deliberately. An agent that wants a box checked should say `check`, not `toggle`, so the outcome does not depend on a state it may have read a moment ago.

### Collections

| Element | Attributes | State | Actions |
| --- | --- | --- | --- |
| `list` | `label`, `multi` | `disabled` | — |
| `item` | `label` | `selected`, `disabled` | `focus` `click` `select` `deselect` |
| `spreadsheet` | `source`, `version`, `rows`, `columns`, `cursor`, `selection` | `disabled` | `focus` `select` `select-range` `type-text` `clear` `submit` |
| `tabs` | `label` | — | — |
| `tab` | `label` | `selected`, `closable`, `movable`, `disabled` | `focus` `select`, plus `close` when `closable` and `move` when `movable` |

**The navigation bar is built from these elements.** Its agentdesk tabs are `tab` elements in a `tabs` strip, written by the compositor as the same markup an application writes, so there is one implementation of a row of tabs and an application gets whatever the bar gets. What the bar has that an application's tabs do not is renaming in place: an agentdesk's name is compositor chrome, kept by the compositor and never told to the workspace, so there is nothing in the protocol for it and an application that wants an editable name renders a `field` and owns it. Switching workspaces is the bar's too, for the same reason.

**`tabs` is a bar, not a row.** Read the navigation bar a pixel at a time, down a column between two of its tabs, and it is: three rows of `raised`, then the tabs standing on `background`, then two rows of `raised`, then one of `border`. The important half is the middle. Its tabs stand on the same colour as the desk behind the bar, and the raised rows are thin edges above and below; painting a full band and standing tabs on it makes them look like buttons lying on a bar rather than tabs cut into one. The strip reproduces that structure and reaches the document's edges, because the bar the screen paints under the navigation document does. An unemphasised `button` fills with `raised` too, so on the band its fill disappears and what is left is text: that is why the navigation bar's tabs read as tabs and its plus reads as a plus. Put the same controls on a window's darker background and they come out as raised chips, a toolbar. The band is the difference, so the element paints it.

Neither a tab nor a menu renders a press, and a tab draws no focus ring either. What they did is visible in what they became: the tab is now the chosen one, the menu is now open, and a flash on top of that reads as a button being clicked. The ring is what makes switching quickly flash: the moment a tab is pressed it takes focus and outlines itself in the accent, and only once the application has answered does it fill, so an outline becomes a fill. The rule lives in the paint, not in either click path, because the navigation bar's tabs are clicked through the compositor's own handler and an application's through `Client::act`; a pair of matching conditions in two places is one edit away from not matching.

A `tab` is then simply a button through the same face every pressable surface wears, with the chosen one carrying `emphasis="primary"` exactly as the navigation bar puts it on the current agentdesk. A tab marked `closable` reserves room at its right end for the cross that closes it, drawn by the same function that draws the navigation bar's, and a press there sends `close` rather than `select`.

A tab marked `movable` can be dragged along its strip, and the slot it lands in is worked out by the same function the navigation bar uses: count the tabs whose middle the pointer has passed. That much is shared. What cannot be is what happens next, because the two orders have different owners. The bar's order is the compositor's, so it rearranges itself under the hand. An application's is the application's, so the compositor says only where the hand put the tab, as a `move` whose value is the slot it should take counting from zero, and the application answers with a new tree, exactly as it answers a table's `scroll`. One event each time the pointer crosses another tab's middle, on the same principle as one event per keystroke: what the application hears is what the hand did, as it does it.

An agent has `move` too, since where a sheet sits in a workbook is the document's business rather than the screen's, unlike a window, which an agent may never arrange. A tab nobody marked `movable` offers no `move`, and asking for one is answered `unsupported-action`.

The strip holds the tabs and whatever else belongs in the bar beside them: the plus that adds one is an ordinary `button`, the way the navigation bar carries its own plus and its stop. Its children being controls in the tree rather than chrome is what makes them addressable — an agent adds a sheet by pressing the plus, and the compositor's own plus, being chrome, is invisible to it. A `menu` is not one of the things that may stand there; see below.

Which content belongs to the chosen tab is the application's business, and it renders that itself; a strip that also owned panels would be the one container in the catalogue deciding what was inside it. A whole strip, as a spreadsheet writes one:

```xml
<tabs gap="sm">
  <tab id="tab-1" label="Sheet 1" selected="true" closable="true" movable="true"
       description="Shows Sheet 1"/>
  <tab id="tab-2" label="Sheet 2" closable="true" movable="true"
       description="Shows Sheet 2"/>
  <button id="new-sheet" label="+" description="Adds a sheet"/>
</tabs>
```

Nothing in there says where the strip is, how tall it is, what colour the band behind it is, or which tab is drawn as the current one beyond `selected`. Those are the compositor's, which is why an application's tabs and the navigation bar's are the same tabs.

`item` has both `click` and `select` because they are different intentions: clicking a file opens it, selecting it marks it for a subsequent operation.

### A spreadsheet's cells are not in the tree

This is the one element whose content does not arrive as markup, and the only place in the system where the whole-tree rule is set aside. It is set aside because a sheet is the first thing that is not small.

Everything else is described by resending the whole document and letting the compositor diff it, which works because everything else *is* small. Cells as elements did not scale in three directions at once. A screenful of a grid is four hundred of them, so an agent paid **46 KB to read twenty numbers**, 89% of it identical action lists and descriptions the compositor had composed itself. Every keystroke made the application re-serialise the visible grid. And a sheet of ten thousand rows could only ever be described a window at a time, so scrolling was a question the application had to answer.

So the tree carries a *name* and a *version*, the way an `image` carries a path, and the cells go up the same socket as their own frames:

```xml
<spreadsheet id="sheet" grow="true" source="book" version="42"
             rows="1000" columns="26" cursor="B7" selection="A1:C5"
             description="The budget"/>
```

```
sheet <source> <version> <base> <at> <value>...
```

One frame shape and no operation verbs: *put these values in, starting at this cell and running across*. A single cell is a run of one, a filled row is a run of many, and emptying a cell is a run carrying an empty string, because in a sheet an empty cell and a cleared one are the same cell.

**Nothing is compared with anything.** The compositor does not work out what changed; it is told, and it writes what it is told. `version` is what the sheet becomes and `base` is what it must already be, so a run that cannot be placed is refused rather than half-applied, and the compositor answers `sheet-resend <source> <have>`. A `base` of zero means "forget what you have and start from here", which is what a snapshot is, so a first publish and a recovery are one code path. `awproto`'s `SheetOut` owns the counter, because getting it wrong is the one way this can go wrong.

A snapshot **carrying no values at all** is how a sheet is said to be empty, and `SheetOut::restart` is exactly that frame. It has to be a frame rather than a flag on the next one: an application that starts a sheet over and then has nothing to publish has said something, and "publish nothing" cannot come out meaning "leave what is there", or an empty sheet becomes the one state that cannot be reached. It is also how a workbook says a sheet is finished, since emptying a stream is the only thing it can say about one it will never write to again.

Ordering is free because it is one connection: publish the cells that make a version, then send the tree that claims it. The compositor can never hold a tree ahead of its data, and because the version is *in the tree*, every render re-asserts which version the picture is of. The compositor checks it: a tree claiming a version it is not on means the picture and the application have come apart, so it answers `sheet-resend` and paints from what comes back, once per disagreement rather than once per render. Divergence is caught on the next frame instead of sitting there.

**A `source` is a sheet, not an application.** A client may name as many as it likes, and the compositor holds one sheet per source whether or not any element currently points at one. That is what makes a workbook cheap: give each sheet a stream of its own and switching tabs is a different `source` in the next tree, with no cells on the wire, nothing diffed and nothing thrown away. Sharing one stream between sheets means every switch is a full resend, and the compositor holds only whichever sheet was published last. Because the compositor keeps a source nothing points at, a name must never be reused for different contents: number them, and let a closed sheet's number die with it.

**Both axes are the compositor's.** It holds the sheet, so scrolling is two offsets rather than a round trip: sixty wheel notches, or a drag of the bar from the top to row 989, move a thousand-row sheet without the application hearing anything at all. What it holds is what the application *chose to publish*, and it scrolls the shape the element declares. An application whose sheet is too large to publish whole has no way yet to be told where the view is, so it cannot stream a band; nothing needs that yet, and it is not claimed.

**Resizing is declaring a different shape.** `rows` and `columns` are on the element and re-asserted by every render, so an application grows a sheet by rendering a bigger one and shrinks it by rendering a smaller one. There is no resize message and there should not be one: it would be a second answer to a question the tree already answers on every frame.

The two directions cost very different things, which is the point of doing it this way.

* **Growing is free at both ends.** A cell's place is arithmetic rather than a node, so a sheet that becomes ten times taller moves nothing, allocates nothing, and is not diffed. The compositor's whole reaction to `rows="1000"` becoming `rows="100000"` is two integer comparisons.
* **Shrinking is where a sheet gives memory back.** A cell outside the shape is not merely scrolled away: the shape is what the application says the sheet *is*, so the compositor drops what falls outside and hands the table back its slack. This is the one place in the system where a client's memory shrinks on its say-so. The pass is over the cells that hold something, not over the shape, so cutting a sheet of a hundred thousand columns to ten costs what it holds rather than what it declares.

Deciding between the two is a pair of comparisons against a bound each sheet keeps as it is written, so the ordinary case — a render re-asserting the shape it already had — walks nothing.

This is safe only because of the publish order. Cells go up before the tree that claims them, so a value written past the old shape is always followed by the shape that makes room for it, and a cut can only ever drop what the application has *just said* is not in the sheet. It follows that an application shrinking a sheet must drop those cells on its own side too: the compositor is not keeping a spare copy for it, and there is no "restore" to ask for short of publishing the sheet again with `base = 0`.

Nothing else moves. A shape is not a delta, so a cut does not touch the version and the application's next run applies exactly as it would have. Inserting or deleting a row *is* a movement of cells, and that is an ordinary publish: there is deliberately no "everything below here slides down" frame, because the sheet can already say where every value ended up.

There is no 26-column limit anywhere: the lettering carries on A, B, … Z, AA, AB as far as it is asked to, and both axes are capped at 100,000 only because the geometry is arithmetic a client chose the inputs to. An application that ships a 26-column sheet chose 26.

**A cell is a coordinate, not a node.** There are no cell elements, so nothing can name one as a target; the cell travels *beside* the action instead, on the event and in an intent. A press on a grid sends `select` naming `B7`; a drag sends `select-range` with the two corners; typing sends `type-text` a keystroke at a time, carrying the value the cell now has. An agent names a cell after its element: `sheet!B7`.

Column letters are the compositor's, because A, B, … Z, AA is what every spreadsheet does and an application saying so would only ever say the same thing. A column's width is the human's to drag and the compositor's to remember, like a scroll offset.

### Choosing a run of cells

`select-range` names one corner in the event's cell and the other in its value. Both must be cells of that grid; anything else is refused.

One action rather than fifteen selects, because a person dragging across a grid did one thing, and because "A1 through C5" is legible in a way a list of ids is not. The human's drag produces the same event, sent again each time the run reaches another cell, in the same spirit as one event per keystroke: the application hears what the hand is doing while it does it. What the run looks like is then the application's business, and it says so by sending `selection` back on the element.

### Reading one

An agent gets the shape in the view and the values by asking:

```xml
<spreadsheet id="sheet" rows="1000" columns="26" cursor="A1" selection="A1"
             used="A1:D500" description="The grid of cells, 1000 rows deep"
             actions="focus select select-range type-text clear submit"/>
```

`used` is the compositor's, the bounding box of everything published, so an agent knows where to look without reading a screenful to find out. `query cells <app> <id> <range>` answers with one line per row, values separated by tabs. The whole view of a sheet application is **14 lines and 1082 bytes**, against 423 lines and 46,650 before.

### Menus and overlays

| Element | Attributes | State | Actions |
| --- | --- | --- | --- |
| `menu` | `label` | `open` | `focus` `open` `close` |
| `menuitem` | `label`, `icon` | `disabled` | `focus` `click` |

**A `menu` is a child of `window` and of nothing else.** A window's menus are its menu bar, and the compositor lays them out as one row across the top of the window, under the title bar it draws and above everything the application put inside; the content stacks below. An application says it has an Edit menu. Where a menu bar goes is not a thing an application gets an opinion about, any more than where its window goes is, so a `menu` anywhere else is a parse error and the document is refused whole.

That rule exists because the first thing anyone did with a menu that could go anywhere was put one in a strip of tabs, where it came out as a tab that was not one: a word sitting in a row of things you choose, that opens instead of choosing. Nothing in the markup was wrong; the markup simply allowed a sentence with no meaning. A closed vocabulary that lets you say that is not closed.

A menu bar, as the same spreadsheet writes one:

```xml
<window title="Sheet">
  <menu id="edit" label="Edit" description="Commands for the chosen cells">
    <menuitem id="menu-fill" label="Fill from the first"
              description="Copies the first chosen cell into the rest"/>
    <menuitem id="menu-clear" label="Clear" description="Empties every chosen cell"/>
  </menu>
  <vstack gap="sm" grow="true">
    ...
  </vstack>
</window>
```

The machinery under it is a dropdown's, deliberately: its items float below it while it is open, painted last and hit first, and fold away to nothing when it is closed, exactly as a `select`'s options do. Both verbs are offered whatever the state, so an intent says what should be true and one that already is does nothing.

**Right-click belongs to the application, not to the vocabulary.** The other mouse button sends a `context` event naming whatever was under it, and that is all it does: what it means is the application's to decide, and what it usually decides is to open a menu. The compositor then hangs that menu's items from where the press landed, which is what makes a context menu appear under the hand. Nothing in the tree says so; the compositor knows because it saw the press, and a press with the ordinary button clears it again.

**Except over words, where the compositor keeps it.** The selection and the clipboard are the compositor's and are never told to anyone, so the menu that acts on them is the compositor's too: the other button over a `field`, an `editor`, or any `text` element opens a Cut / Copy / Paste of its own, and the application hears nothing. Over a `spreadsheet` the press stays the application's, because cut and copy over a grid mean *cells*, and which cells are chosen is the application's state, published by it and sent back on the element. Neither menu is in any tree and neither reaches an agent, which has no other button and needs none.

An agent has no right-click and needs none: it opens a menu by naming it, which reaches the same commands without a pointer. A menu with no label draws no title in the bar and still holds a place in the tree, which is how an application offers a context menu that is not also a menu the human can pull down.
| `dialog` | `label` | — | — |
| `popover` | `label` | `open` | `close` |

A `dialog` is a container an application puts in its tree when it has a question, and takes out when the question is answered: there is no `open` state and no `dismiss` action, because the application decides both by what it renders, and its own Cancel button is what dismissal is. The compositor floats it centred over the window and makes it **modal**: while one is in the tree, every control outside it is visible but inert, for the human and the agent alike. The human's clicks do not reach them, the keyboard follows focus into the dialog, and in the agent's view they carry `blocked` with an empty action list; an intent naming one is rejected as `blocked`, told apart from `disabled` so the agent knows to look for the dialog rather than wait for the application. To the agent a dialog is nothing special: controls nested in `<dialog label="...">`, read and acted on like any others. This is a genuine safety property, not a rendering detail. It stops an agent acting on a screen the human has been interrupted away from, and it means an agent that has learned one dialog has learned them all.

`awkit` holds the shared ones: `FileDialog` for a file to open, a name to save under, or a folder; `Confirm` for a yes-or-no; `TextPrompt` for a line of text. Each is a model every application embeds the same way, so choosing a file, confirming a deletion or naming a folder is the same clicks in every application that asks.

## The Action Vocabulary

The complete closed set. Nothing else exists.

| Action | Parameter | Meaning |
| --- | --- | --- |
| `focus` | — | Move keyboard focus here. |
| `click` | — | Primary activation. |
| `type-text` | text | Replace the contents with this text. |
| `clear` | — | Empty the contents. |
| `submit` | — | Confirm, as the Enter key would. |
| `check` | — | Make checked, whatever it was. |
| `uncheck` | — | Make unchecked, whatever it was. |
| `toggle` | — | Invert. |
| `select` | — | Make this the selected one. |
| `select-range` | the other corner | Choose a run of cells between two corners of a grid. |
| `deselect` | — | Remove from the selection. |
| `set-value` | number | Set a numeric value. |
| `open` | — | Expand a menu, dropdown or popover. |
| `close` | — | Collapse it. |


### Scrolling is not in the vocabulary

There is no verb for it, and an agent is never told something was out of view.

Acting on a node scrolls whatever has to move first: every scroll container above it, outermost first, and a grid to the cell being acted on. This is the same thing arrangement does one level up, where acting on an application brings its window forward and puts its siblings away. Reachability is something the compositor makes true, not a question it answers.

The rejection that used to exist for this, `not-visible`, is gone. It was a symptom wearing the name of one of its causes: a nested container the old code did not walk, a grid it walked straight past, a folded dropdown option whose rectangle happened to be empty. Each of those is either a bug here or a different word. What remains is `unreachable`, which an agent should never see: it means the compositor tried to reveal a node and failed, and it is logged as the fault it is.

There used to be one thing revealing could not fix — a row the application had not sent — and there no longer is. The compositor holds the sheet, so a cell five hundred rows down is two offsets away rather than a question.

## Selection and the clipboard

Neither is in the protocol, in either direction.

Highlighting is the human's: a press anchors, a drag extends, and the run between is painted behind the words. Ctrl+A, Ctrl+C, Ctrl+X and Ctrl+V are recognised by the compositor against its own copy of a text control, so what an application receives from a paste is a `type-text` carrying the value the control now has, exactly what it would have received had the human typed the words out. No application needs to know a clipboard exists, and none can read one it was not given. No modifier ever reaches the event vocabulary: the keyboard layer turns the chord into the intention before anything else sees it.

**There is one text box on this machine**, `haimanager/src/text.rs`, and an application's `field`, an `editor`, a spreadsheet cell being typed into, the start menu's prompt and the navigation bar's rename field are all it. That is worth stating because it was not true: the two chrome boxes had `push` and `pop` and nothing else, so neither could be selected in, copied out of, pasted into, or have its caret moved by an arrow key. A text box that cannot be pasted into is not a text box, and there was no reason for these to be a different thing from the others. What each of them keeps for itself is only what Enter means, which is the one thing that genuinely differs: a field submits, an editor breaks the line, the prompt makes a workspace, the rename field commits a name.

What the one text box does, everywhere:

* A press puts the caret down and anchors a run; a drag extends it.
* **A second press in the same place takes the word under it, and a third takes the line.** Letters, digits and underscores are one word, a run of spaces is one thing, and punctuation is taken a character at a time, so a double click on `foo.bar` takes `foo`.
* **Shift with an arrow drags a run out from the keyboard**, from wherever the caret is and whether or not anything is selected yet: the first such keystroke is what anchors the run. Home and End go to the ends of the line, with shift held or without. An arrow without shift collapses a run to the end it was moving toward rather than to wherever the caret happened to be.
* Backspace eats back, Delete eats forward, and either takes the whole run when there is one.
* Ctrl+A, Ctrl+C, Ctrl+X, Ctrl+V.

Static `text` is selectable too, and that is not the same machinery, because a paragraph on a page is not a control: it has no value an application tracks, no caret, and nothing that can be typed into it. What it has is a run, started by pressing on the words, taken whole by a double or triple click, reached further with shift and an arrow, and copied with Ctrl+C. A press on words is the only way to say where such a run begins, since there is no caret to put down in a paragraph, so a press that selects nothing is kept as an empty run rather than dropped: it paints nothing, copies nothing, and is what the first shift reaches out from. A run stops at one element, so an agent's answer selects whole and an answer plus the label above it does not.

**Shift with an arrow over a grid means cells, not characters.** A chosen cell has no caret, so there is nothing in it for a run of text to be; what the keystroke means there is the same thing it means in every spreadsheet, one cell further, and it ends in the same `select-range` a drag across the grid sends. The far corner is remembered between keystrokes, so shift and right twice reaches two cells. A plain arrow is how it stops being a run, and it moves the *application's* cursor by sending it the `select` a press would have sent. A cell being typed into is a text box again, and then shift with left or right is characters; up and down stay the grid's, exactly as the plain ones do, because a spreadsheet that trapped the cursor in a half-typed cell would be unusable.

## Reading a sheet

A `spreadsheet`'s cells are not in the view, because a screenful of a grid is four hundred of them and four hundred elements is forty kilobytes to learn twenty numbers. The element says how far the sheet runs, where the cursor is, what is chosen, and `used`, the rectangle anything has been put in. To read the part that matters:

```
query cells <app> <id> <range>
```

`range` is `A1:D20`, or one cell as `B7`. The answer is one line per row, values separated by tabs. A range that runs past the sheet is cut to it, because an agent asking for `A1:Z100` of a small sheet is asking to see the sheet.

It is a **query rather than an action** because it is a read: the agent wants to see part of a sheet, not change anything. And unlike the `query rows` it replaced, nothing is asked of the application at all — the compositor holds the cells, so the answer is a lookup and arrives at once.

## Selection and the clipboard

An agent may **read** the clipboard and may not write one. Reading is how it learns what the human just copied, which is context it has no other way to get. There is nothing to write, because an agent that wants text somewhere says so with `type-text` rather than putting it down and picking it up again. The answer carries a kind as well as its content: `none`, `text`, or `image` for a picture as base64. Nothing produces an image yet; the kind exists so that the day something does, a reader that only understands words says it cannot read it rather than printing base64 as if it were words.

## What the Agent Actually Sees

The same interface, as the app writes it and as the agent receives it.

**What the app emits:**

```xml
<window title="Messages">
  <vstack gap="md">
    <text role="heading">Compose</text>
    <field id="to" label="To" placeholder="name@example.com"
           description="Recipient address for the message" value=""/>
    <editor id="body" label="Message"
            description="Body text of the message being composed" value=""/>
    <hstack gap="sm">
      <button id="send" label="Send" emphasis="primary" disabled
              description="Sends the composed message to its recipient"/>
      <button id="discard" label="Discard" emphasis="danger"
              description="Throws away the draft without sending it"/>
    </hstack>
  </vstack>
</window>
```

**What the agent receives:**

```xml
<view app="Messages" desk="3">
  <text>Compose</text>
  <field id="to" value="" description="Recipient address for the message"
         actions="focus type-text clear submit"/>
  <editor id="body" value="" description="Body text of the message being composed"
          actions="focus type-text clear"/>
  <button id="send" disabled description="Sends the composed message to its recipient"
          actions=""/>
  <button id="discard" description="Throws away the draft without sending it"
          actions="focus click"/>
</view>
```

Four things happened:

* **Both stacks are gone.** Layout is not semantics. Their children flattened upward.
* **Appearance is gone.** `label` survives because it is what the control *says*; `placeholder`, `emphasis`, `font`, `size`, `weight`, `italic` and `color` do not.
* **Actions appeared.** Derived from type and state, not copied from the app.
* **The disabled button kept its entry but has no actions.** The agent can see it exists and why it might matter, and can see it cannot be used yet. Removing it entirely would leave the agent unable to reason about what it is waiting for.

## Intents

An agent acts by naming a node and an action. It never produces an event.

```xml
<intent action="type-text" target="to" value="alice@example.com"/>
<intent action="click" target="discard"/>
```

The haimanager resolves the target to a rectangle, verifies it is visible, hit-testable and enabled, animates the fake cursor to it, and only then synthesizes the event a human click would have produced. Rejections are answers, not silence:

```xml
<rejected target="send" reason="disabled"/>
<rejected target="row-88" reason="blocked"/>
<rejected target="purchase" reason="needs-approval"/>
<rejected target="send-message" reason="not-addressable"/>
```

`not-addressable` covers anything the agent may not touch: another workspace's apps, and the agentdesk's own chrome. Both are enforced by which connection the request arrived on, never by a claim the agent makes about itself.

## Identity Rules

Two requirements on `id` that are easy to get wrong and expensive to debug.

**Stable across re-renders.** Apps resend their whole tree on every change and the haimanager diffs it. Matching old node to new node is by `id`. An app that regenerates ids per frame silently moves the human's text cursor on every keystroke and moves the agent's target out from under it between reading a screen and acting on it.

**Unique within a window.** Not globally. An agent addresses a node within an app it has already named, so two apps may both have a `send`.

Ids should be meaningful (`send`, `recipient-field`) rather than positional (`btn-3`). They appear in the agent's reasoning, and a model does better with a name that means something.

## Scope for Version One

The full catalogue above is the target. Sixteen elements are enough to build the whole agentdesk shell and the first real applications:

```
vstack  hstack  scroll  text  divider  icon  image
button  field  editor  checkbox  select  option
list  item  dialog  table  column  row  cell
```

Everything else is additive. Nothing in the reduced set forecloses the rest, and no element should be built before an application actually needs it.

`slider` and `tabs` are the ones most likely to be wanted next. `slider` needs drag tracking, for which the column edges and the scrollbars are now the pattern. `select` is built; its overlay is the same machinery a dialog's is. `table` is built, along with the two things it needed that nothing else did: a second scrolling axis, and a window over content too large to send.
