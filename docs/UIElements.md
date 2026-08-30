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
| `table` | `label`, `rows`, `first-row` | `disabled` | — |
| `column` | `label`, `chars` | — | — |
| `row` | `label` | `selected` | `focus` `select` |
| `cell` | — | `value`, `editable`, `selected`, `disabled` | `focus` `select`, plus `type-text` `clear` `submit` when `editable` |
| `tabs` | `label` | — | — |
| `tab` | `label` | `selected`, `disabled` | `focus` `select` |

`item` has both `click` and `select` because they are different intentions: clicking a file opens it, selecting it marks it for a subsequent operation.

### A table holds a window, not a sheet

A `cell` is a control, not content: a spreadsheet's cells are named, read and typed into one at a time, and something that cannot be addressed cannot be any of those. It is the one control whose description is not written by the application, because its meaning is entirely positional and a sheet has far too many of them for a sentence each; the compositor composes one from the column's label and the row's, so `A1` reads as "The cell in column A, 1". There is no `click` on a cell. Pressing one means choosing it, and `select` is the event a person pressing it produces; pressing an already-chosen editable cell puts the caret in, which is what a double click means elsewhere, spread over two presses because there is no double click in the event vocabulary.

**The `row` children are the window, not the sheet.** `rows` says how many rows exist altogether and `first-row` which row the first child is, so a sheet of ten thousand rows is described forty at a time and the tree stays the size of the screen. Nothing else in the system works this way, and nothing else needs to: an application resends everything because everything is small. A spreadsheet is the first thing that is not.

Two axes with two owners follow from that. Across is the compositor's, an ordinary offset over columns that are all present. Down is the application's: the compositor draws the bar against `rows`, and the wheel, the bar and an agent's `query rows` all become a `scroll` event carrying the row that should now be first. The application answers by re-rendering with a new `first-row`, exactly as a dropdown answers `open` by re-rendering with its options. It is the only scroll position that crosses the protocol, and it crosses because it is not a scroll position: it is which slice of the sheet the application chose to describe.

A column's width is in `chars`, not pixels, because a column's width is a property of what is in it and a pixel count would be a different column on a different display. The human may drag a column's edge, and where they drag it to is the compositor's, like a scroll offset: the application declares where a column starts and is never told it moved.

### Menus and overlays

| Element | Attributes | State | Actions |
| --- | --- | --- | --- |
| `menu` | `label` | `open` | `open` `close` |
| `menuitem` | `label`, `icon` | `disabled` | `focus` `click` |
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
| `deselect` | — | Remove from the selection. |
| `set-value` | number | Set a numeric value. |
| `open` | — | Expand a menu, dropdown or popover. |
| `close` | — | Collapse it. |


### Scrolling is not in the vocabulary

There is no verb for it, and an agent is never told something was out of view.

Acting on a node scrolls whatever has to move first: every scroll container above it, outermost first, and a table across its columns. This is the same thing arrangement does one level up, where acting on an application brings its window forward and puts its siblings away. Reachability is something the compositor makes true, not a question it answers.

The rejection that used to exist for this, `not-visible`, is gone. It was a symptom wearing the name of one of its causes: a nested container the old code did not walk, a table it walked straight past, a folded dropdown option whose rectangle happened to be empty. Each of those is either a bug here or a different word. What remains is `unreachable`, which an agent should never see: it means the compositor tried to reveal a node and failed, and it is logged as the fault it is.

A row nobody sent is the one thing revealing cannot fix, and that is not scrolling. See `query rows` below.

## Selection and the clipboard

Neither is in the protocol, in either direction.

Highlighting is the human's: a press anchors, a drag extends, and the run between is painted behind the words. Ctrl+A, Ctrl+C, Ctrl+X and Ctrl+V are recognised by the compositor against its own copy of a text control, so what an application receives from a paste is a `type-text` carrying the value the control now has, exactly what it would have received had the human typed the words out. No application needs to know a clipboard exists, and none can read one it was not given. No modifier ever reaches the event vocabulary: the keyboard layer turns the chord into the intention before anything else sees it.

## Reading a collection too large to send

A table holds a window. An agent reading a thousand-row sheet sees the forty rows the application described, which is what a human sees too, and `rows` and `first-row` in its view tell it that is what it is holding.

To read a different part it asks:

```
query rows <app> <table> <first-row>
```

The compositor asks the application to move that table's window and answers with the view once it has, within a bounded wait. One call both asks and reads.

It is a **query rather than an action** for two reasons. It is a read: the agent wants to see a part of a collection, not change anything. And there is nothing to act on — a cell outside the window is not in the tree, so an intent naming it has no target to resolve. An application that ignores the request costs the agent one stale view whose `first-row` says plainly that nothing moved.

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
