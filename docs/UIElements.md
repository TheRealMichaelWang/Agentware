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
| `scroll` | `dir=x\|y\|both`, `anchor=end` | Clips and scrolls its overflow. Needs no id: see `scroll-into-view`. `anchor="end"` keeps the end in view as content grows, until the human scrolls away from it: a transcript's behaviour. |
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
| `divider` | `dir` | Purely visual, but cheap to keep. |
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
| `select` | `label` | `value`, `disabled`, `open` | `focus` `open` `close` |
| `option` | `label`, `value` | `selected`, `disabled` | `select` |
| `slider` | `label`, `min`, `max`, `step` | `value`, `disabled` | `focus` `set-value` |

`check` and `uncheck` exist alongside `toggle` deliberately. An agent that wants a box checked should say `check`, not `toggle`, so the outcome does not depend on a state it may have read a moment ago.

### Collections

| Element | Attributes | State | Actions |
| --- | --- | --- | --- |
| `list` | `label`, `multi` | `disabled` | — |
| `item` | `label` | `selected`, `disabled` | `focus` `click` `select` `deselect` |
| `table` | `label` | `disabled` | — |
| `column` | `label` | — | — |
| `row` | — | `selected` | `focus` `select` |
| `cell` | — | — | — |
| `tabs` | `label` | — | — |
| `tab` | `label` | `selected`, `disabled` | `focus` `select` |

`item` has both `click` and `select` because they are different intentions: clicking a file opens it, selecting it marks it for a subsequent operation.

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
| `scroll-into-view` | — | Make a node visible. Any node, not just controls. |

### `scroll-into-view` is special

The haimanager rejects an intent naming a node that is not visible, because an agent must not be able to act on something a human could not have clicked. That would leave an agent stuck the moment its target scrolled off screen.

`scroll-into-view` is the way out, and it targets **any node**, control or not. It is also the one action handled entirely by the haimanager: the agent says which node it wants visible, and the haimanager works out which container to scroll and by how much. The agent never addresses a scroll container, which is why `scroll` needs no id.

This is the intent principle applied to a mechanism: the agent expresses what it wants to be true, not the steps to make it so.

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
<rejected target="row-88" reason="not-visible"/>
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

The full catalogue above is the target. Fourteen elements are enough to build the whole agentdesk shell and the first real applications:

```
vstack  hstack  scroll  text  divider  icon  image
button  field  editor  checkbox
list  item  dialog
```

Everything else is additive. Nothing in the reduced set forecloses the rest, and no element should be built before an application actually needs it.

`select`, `slider`, `table` and `tabs` are the ones most likely to be wanted next, in that order. Each needs renderer machinery the first twelve do not: overlay positioning and focus trapping for `select` and `menu`, drag tracking for `slider`, column sizing for `table`.
