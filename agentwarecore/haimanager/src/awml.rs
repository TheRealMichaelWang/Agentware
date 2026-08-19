//! AWML: the markup applications describe their interfaces in.
//!
//! See `docs/UIElements.md` for the catalogue. The rule that shapes everything
//! here is that markup describes **affordances, not appearance**: an app says
//! "a button, id `send`, enabled, which sends the message", never "a blue
//! rectangle". The haimanager decides what that looks like.
//!
//! The parser accepts a deliberately small subset of XML shapes: elements,
//! attributes, self-closing tags and text content. No comments, no namespaces,
//! no processing instructions, no DTDs. A closed element vocabulary is only
//! worth having if the syntax around it is closed too.
//!
//! Nodes live in an arena rather than owning their children. Layout produces a
//! `Vec<Rect>` indexed the same way, hit testing is a scan of that vector, and
//! nothing needs a lifetime or a reference count to say "the node at this
//! rectangle".

use std::fmt;

/// Every element that exists. Anything else is a parse error, which is the
/// point: it keeps an agent's action space finite and enumerable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tag {
    // Structure. Stripped from the agent's view, children flattened upward.
    Window,
    VStack,
    HStack,
    Scroll,
    // Semantic structure. Survives, because it carries meaning rather than
    // arrangement.
    Group,
    // Content. Visible to agents for reading; no id, no actions.
    Text,
    Divider,
    Icon,
    Image,
    // Controls. Require an id and a description; accept actions.
    Button,
    Field,
    Editor,
    Checkbox,
    List,
    Item,
    /// A dropdown: one chosen value, the options shown only while `open`.
    Select,
    Option,
    Dialog,
}

impl Tag {
    pub fn parse(name: &str) -> Option<Tag> {
        Some(match name {
            "window" => Tag::Window,
            "vstack" => Tag::VStack,
            "hstack" => Tag::HStack,
            "scroll" => Tag::Scroll,
            "group" => Tag::Group,
            "text" => Tag::Text,
            "divider" => Tag::Divider,
            "icon" => Tag::Icon,
            "image" => Tag::Image,
            "button" => Tag::Button,
            "field" => Tag::Field,
            "editor" => Tag::Editor,
            "checkbox" => Tag::Checkbox,
            "list" => Tag::List,
            "item" => Tag::Item,
            "select" => Tag::Select,
            "option" => Tag::Option,
            "dialog" => Tag::Dialog,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Tag::Window => "window",
            Tag::VStack => "vstack",
            Tag::HStack => "hstack",
            Tag::Scroll => "scroll",
            Tag::Group => "group",
            Tag::Text => "text",
            Tag::Divider => "divider",
            Tag::Icon => "icon",
            Tag::Image => "image",
            Tag::Button => "button",
            Tag::Field => "field",
            Tag::Editor => "editor",
            Tag::Checkbox => "checkbox",
            Tag::List => "list",
            Tag::Item => "item",
            Tag::Select => "select",
            Tag::Option => "option",
            Tag::Dialog => "dialog",
        }
    }

    /// Pure arrangement. Invisible to agents; children flatten through it.
    pub fn is_layout(self) -> bool {
        matches!(self, Tag::Window | Tag::VStack | Tag::HStack | Tag::Scroll)
    }

    /// Takes actions, and therefore requires an id and a description.
    pub fn is_control(self) -> bool {
        matches!(
            self,
            Tag::Button | Tag::Field | Tag::Editor | Tag::Checkbox | Tag::Item | Tag::Select | Tag::Option
        )
    }

    /// What an agent may do with this element, given its state.
    ///
    /// Derived here rather than declared by the app. If applications wrote
    /// their own action lists they could omit `focus`, invent something
    /// nothing implements, or advertise `click` on a disabled control, and the
    /// action space would stop being closed.
    pub fn actions(self, disabled: bool) -> &'static [&'static str] {
        if disabled {
            return &[];
        }
        match self {
            Tag::Button => &["focus", "click"],
            Tag::Field => &["focus", "type-text", "clear", "submit"],
            Tag::Editor => &["focus", "type-text", "clear"],
            Tag::Checkbox => &["focus", "check", "uncheck", "toggle"],
            Tag::Item => &["focus", "click", "select", "deselect"],
            // A dropdown offers both verbs whatever its state, the way a
            // checkbox offers check and uncheck: the intent says what should
            // be true, and one that is already true is a no-op, not an error.
            Tag::Select => &["focus", "open", "close"],
            Tag::Option => &["select"],
            _ => &[],
        }
    }
}

pub struct Node {
    pub tag: Tag,
    pub attrs: Vec<(String, String)>,
    /// Text content, with surrounding whitespace collapsed.
    pub text: String,
    pub children: Vec<usize>,
    /// The enclosing element, or `None` for the root. Styling inherits, so
    /// resolving how a node is drawn means walking up until an ancestor has an
    /// opinion.
    pub parent: Option<usize>,
}

impl Node {
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    /// A valueless attribute means true, as `disabled` does in HTML. An
    /// explicit `="false"` still means false.
    pub fn flag(&self, name: &str) -> bool {
        match self.attr(name) {
            Some("false") | Some("0") => false,
            Some(_) => true,
            None => false,
        }
    }

    pub fn id(&self) -> Option<&str> {
        self.attr("id")
    }

    pub fn disabled(&self) -> bool {
        self.flag("disabled")
    }

    /// Overwrite an attribute, adding it if the element did not carry one.
    ///
    /// Used for exactly one thing: writing the compositor's own copy of a text
    /// control's value back into the held tree, so that what is painted and what
    /// an agent reads are the same string even while the application has not yet
    /// caught up with what the human typed.
    pub fn set(&mut self, name: &str, value: &str) {
        match self.attrs.iter_mut().find(|(key, _)| key == name) {
            Some((_, existing)) => *existing = value.to_owned(),
            None => self.attrs.push((name.to_owned(), value.to_owned())),
        }
    }
}

pub struct Tree {
    pub nodes: Vec<Node>,
}

impl Tree {
    pub const ROOT: usize = 0;

    pub fn node(&self, index: usize) -> &Node {
        &self.nodes[index]
    }

    /// The dialog in front, if the document has one open.
    ///
    /// A `dialog` is modal: while one is in the tree, everything outside it is
    /// visible but inert, for the human and the agent alike. The last one in
    /// document order is the one in front, so an app that opens a second on
    /// top of the first gets what it would expect. There is no attribute for
    /// this and no way to opt out, because a dialog that could be clicked
    /// around is a dialog an agent could ignore, and the point of one is that
    /// the choice it asks for is made before anything else happens.
    pub fn modal(&self) -> Option<usize> {
        (0..self.nodes.len())
            .rev()
            .find(|&index| self.nodes[index].tag == Tag::Dialog)
    }

    /// Every dropdown showing its options, in document order.
    ///
    /// Their options float over whatever follows them, so layout, painting and
    /// hit testing each want the list.
    pub fn open_selects(&self) -> Vec<usize> {
        (0..self.nodes.len())
            .filter(|&index| self.nodes[index].tag == Tag::Select && self.nodes[index].flag("open"))
            .collect()
    }

    /// Whether a node is an option of a dropdown that is not showing them.
    /// Such a node has no place on screen and answers to nothing.
    pub fn folded(&self, index: usize) -> bool {
        let node = &self.nodes[index];
        if node.tag != Tag::Option {
            return false;
        }
        node.parent.is_none_or(|parent| !self.nodes[parent].flag("open"))
    }

    /// Whether a node is behind an open dialog: outside it while it is up.
    pub fn blocked(&self, index: usize) -> bool {
        match self.modal() {
            Some(front) => !self.within(index, front),
            None => false,
        }
    }

    /// Whether `index` is `ancestor` or a descendant of it.
    pub fn within(&self, index: usize, ancestor: usize) -> bool {
        let mut at = Some(index);
        while let Some(node) = at {
            if node == ancestor {
                return true;
            }
            at = self.nodes[node].parent;
        }
        false
    }

    /// Whether a node can take an action right now: not disabled by the
    /// application, and not behind a dialog. The single predicate every
    /// path checks, so a human's click, an agent's intent and the agent's
    /// view cannot disagree about it.
    pub fn inert(&self, index: usize) -> bool {
        self.nodes[index].disabled() || self.blocked(index) || self.folded(index)
    }

    /// The nearest value of an inheriting attribute, searching up the tree.
    ///
    /// Styling inherits so a window can set a font once rather than every
    /// element repeating it. The nearest ancestor wins, which is what makes a
    /// local override work.
    pub fn inherited(&self, mut index: usize, name: &str) -> Option<&str> {
        loop {
            if let Some(value) = self.nodes[index].attr(name) {
                return Some(value);
            }
            index = self.nodes[index].parent?;
        }
    }

}

#[derive(Debug)]
pub struct Error {
    pub message: String,
    pub offset: usize,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} at byte {}", self.message, self.offset)
    }
}

/// Parse a document into a tree.
///
/// Exactly one root element is required, and it must be a `window`. A document
/// with two roots or none is a bug in the client rather than something to
/// recover from.
pub fn parse(source: &str) -> Result<Tree, Error> {
    Parser { bytes: source.as_bytes(), at: 0, nodes: Vec::new() }.document()
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
    nodes: Vec<Node>,
}

impl<'a> Parser<'a> {
    fn fail<T>(&self, message: impl Into<String>) -> Result<T, Error> {
        Err(Error { message: message.into(), offset: self.at })
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    fn skip_space(&mut self) {
        while matches!(self.peek(), Some(b) if b.is_ascii_whitespace()) {
            self.at += 1;
        }
    }

    fn document(mut self) -> Result<Tree, Error> {
        self.skip_space();
        let root = self.element()?;
        if root != Tree::ROOT {
            return self.fail("internal: root is not node zero");
        }

        self.skip_space();
        if self.at < self.bytes.len() {
            return self.fail("trailing content after the root element");
        }
        if self.nodes[Tree::ROOT].tag != Tag::Window {
            return self.fail("the root element must be <window>");
        }
        Ok(Tree { nodes: self.nodes })
    }

    /// Parse one element and everything inside it, returning its arena index.
    fn element(&mut self) -> Result<usize, Error> {
        if self.peek() != Some(b'<') {
            return self.fail("expected an element");
        }
        self.at += 1;

        let name = self.name()?;
        let Some(tag) = Tag::parse(&name) else {
            return self.fail(format!("unknown element <{name}>"));
        };

        let attrs = self.attributes()?;
        let index = self.nodes.len();
        self.nodes.push(Node {
            tag,
            attrs,
            text: String::new(),
            children: Vec::new(),
            parent: None,
        });

        self.skip_space();
        if self.peek() == Some(b'/') {
            self.at += 1;
            if self.peek() != Some(b'>') {
                return self.fail("expected '>' after '/'");
            }
            self.at += 1;
            return Ok(index);
        }

        if self.peek() != Some(b'>') {
            return self.fail("expected '>'");
        }
        self.at += 1;

        self.content(index, tag, &name)?;
        Ok(index)
    }

    /// Children and text, up to the matching close tag.
    fn content(&mut self, index: usize, tag: Tag, name: &str) -> Result<(), Error> {
        // Collected as bytes and decoded once at the end. Pushing each byte as
        // a char reads multi-byte UTF-8 as Latin-1, which turned every non-ASCII
        // character in element text into two accented ones: `÷` arrived as `Ã·`.
        // Attribute values never had the bug, which is why it survived until an
        // application put a non-ASCII string in text content.
        let mut text = Vec::new();

        loop {
            let Some(byte) = self.peek() else {
                return self.fail(format!("unclosed <{name}>"));
            };

            if byte != b'<' {
                text.push(byte);
                self.at += 1;
                continue;
            }

            if self.bytes.get(self.at + 1) == Some(&b'/') {
                self.at += 2;
                let closing = self.name()?;
                if closing != name {
                    return self.fail(format!("</{closing}> closes <{name}>"));
                }
                self.skip_space();
                if self.peek() != Some(b'>') {
                    return self.fail("expected '>' to end a closing tag");
                }
                self.at += 1;

                // The parser's input arrived as `&str`, so this never actually
                // loses anything; lossy only so a slicing bug shows up as ?
                // on screen rather than a crash in the compositor.
                // Entities decode in text the same as in an attribute value. A
                // client escapes what a human typed before it becomes markup,
                // and `&lt;` on screen where the human typed `<` would be the
                // escape showing rather than the text.
                let collapsed = unescape(&collapse(&String::from_utf8_lossy(&text)));
                // Text belongs to the element that contains it, not to a
                // synthetic child. Mixed content is not supported: an element
                // has text or children, and using both is a client bug that
                // would render ambiguously.
                if !collapsed.is_empty() {
                    if !self.nodes[index].children.is_empty() {
                        return self.fail(format!("<{name}> mixes text and child elements"));
                    }
                    self.nodes[index].text = collapsed;
                }
                let _ = tag;
                return Ok(());
            }

            let child = self.element()?;
            self.nodes[child].parent = Some(index);
            self.nodes[index].children.push(child);
        }
    }

    fn name(&mut self) -> Result<String, Error> {
        let start = self.at;
        while matches!(self.peek(), Some(b) if b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            self.at += 1;
        }
        if self.at == start {
            return self.fail("expected a name");
        }
        Ok(String::from_utf8_lossy(&self.bytes[start..self.at]).into_owned())
    }

    fn attributes(&mut self) -> Result<Vec<(String, String)>, Error> {
        let mut attrs = Vec::new();

        loop {
            self.skip_space();
            match self.peek() {
                Some(b'>') | Some(b'/') | None => return Ok(attrs),
                _ => {}
            }

            let key = self.name()?;
            self.skip_space();

            // A bare attribute is true, so `disabled` reads the way it does in
            // every other markup language.
            if self.peek() != Some(b'=') {
                attrs.push((key, "true".to_owned()));
                continue;
            }
            self.at += 1;
            self.skip_space();

            let Some(quote) = self.peek() else {
                return self.fail("expected a quoted value");
            };
            if quote != b'"' && quote != b'\'' {
                return self.fail("attribute values must be quoted");
            }
            self.at += 1;

            let start = self.at;
            while let Some(byte) = self.peek() {
                if byte == quote {
                    break;
                }
                self.at += 1;
            }
            if self.peek() != Some(quote) {
                return self.fail("unterminated attribute value");
            }
            let raw = String::from_utf8_lossy(&self.bytes[start..self.at]).into_owned();
            self.at += 1;

            attrs.push((key, unescape(&raw)));
        }
    }
}

/// Collapse runs of whitespace and trim, so indentation in the source does not
/// become spacing on screen.
fn collapse(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The five XML entities. Nothing else is recognised, and an unknown entity is
/// left alone rather than being an error: user text containing a stray `&` is
/// far more likely than a client meaning something by it.
fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }

    let mut out = String::with_capacity(text.len());
    let mut rest = text;

    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        rest = &rest[start..];

        let Some(end) = rest.find(';').filter(|&end| end <= 6) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };

        match &rest[..=end] {
            "&amp;" => out.push('&'),
            "&lt;" => out.push('<'),
            "&gt;" => out.push('>'),
            "&quot;" => out.push('"'),
            "&apos;" => out.push('\''),
            other => out.push_str(other),
        }
        rest = &rest[end + 1..];
    }

    out.push_str(rest);
    out
}

/// The same tree, as an agent sees it.
///
/// This is the payoff of the whole design. There is no second tree and no
/// derivation from a rendered image: the human's pixels and the agent's
/// semantics come out of one document, so they cannot drift. Every other system
/// builds a render tree and derives an accessibility tree from it, which is why
/// accessibility trees are perpetually stale.
///
/// Three things are dropped, and each for a stated reason:
///
/// * **Layout elements**, entirely, with their children flattened upward. An
///   agent does not need to know two buttons sit in a row.
/// * **Appearance**: `label` survives as content because it is what the control
///   says, but `emphasis`, `gap` and `placeholder` do not.
/// * **Nothing else.** State stays, because `disabled` is what makes an intent
///   rejectable, and descriptions stay because they are the only thing telling
///   an agent what a control means.
///
/// `actions` is added, computed from the element and its state rather than
/// copied from the document.
pub fn agent_view(tree: &Tree, app: &str, desk: u32) -> String {
    // The app name and the workspace come from the supervisor's handoff, not
    // from the document, so an application cannot misreport which workspace it
    // is in or borrow another one's name.
    let mut out = format!("<view app=\"{app}\" desk=\"{desk}\">\n");
    emit(tree, Tree::ROOT, 1, &mut out);
    out.push_str("</view>");
    out
}

fn emit(tree: &Tree, index: usize, depth: usize, out: &mut String) {
    let node = tree.node(index);

    // Arrangement carries no meaning, so it does not appear; its children rise
    // to take its place.
    if node.tag.is_layout() {
        for &child in &node.children {
            emit(tree, child, depth, out);
        }
        return;
    }

    let pad = "  ".repeat(depth);

    if node.tag == Tag::Divider {
        return;
    }

    if node.tag == Tag::Text {
        out.push_str(&format!("{pad}<text>{}</text>\n", node.text));
        return;
    }

    let mut attrs = String::new();
    if let Some(id) = node.id() {
        attrs.push_str(&format!(" id=\"{id}\""));
    }
    // The label is what the control says, which an agent needs; how it is
    // emphasised is not.
    if let Some(label) = node.attr("label") {
        attrs.push_str(&format!(" label=\"{label}\""));
    }
    // A picture's words. An agent that cannot see the picture must still know
    // what it shows, which is why `alt` is required on one.
    if let Some(alt) = node.attr("alt") {
        attrs.push_str(&format!(" alt=\"{alt}\""));
    }
    // A password's value is masked here exactly as it is on screen: the agent
    // reads the same dots the human sees, never the contents. Without this the
    // API key entered in Settings would be readable by the very agent it
    // authenticates, through the view of any app that renders it.
    let password = node.attr("kind") == Some("password");
    for state in ["value", "checked", "selected", "open", "invalid", "busy"] {
        if let Some(value) = node.attr(state) {
            if password && state == "value" {
                let masked = "*".repeat(value.chars().count());
                attrs.push_str(&format!(" value=\"{masked}\""));
            } else {
                attrs.push_str(&format!(" {state}=\"{value}\""));
            }
        }
    }
    if node.disabled() {
        attrs.push_str(" disabled");
    }
    // Behind an open dialog. Not the application's doing, so a separate word
    // from `disabled`: the control is fine, something is in front of it, and
    // dealing with the dialog is what brings it back.
    if node.tag.is_control() && tree.blocked(index) {
        attrs.push_str(" blocked");
    }
    if let Some(description) = node.attr("description") {
        attrs.push_str(&format!(" description=\"{description}\""));
    }
    if node.tag.is_control() {
        let actions = node.tag.actions(tree.inert(index)).join(" ");
        attrs.push_str(&format!(" actions=\"{actions}\""));
    }

    let name = node.tag.name();
    if node.children.is_empty() {
        out.push_str(&format!("{pad}<{name}{attrs}/>\n"));
        return;
    }

    out.push_str(&format!("{pad}<{name}{attrs}>\n"));
    for &child in &node.children {
        emit(tree, child, depth + 1, out);
    }
    out.push_str(&format!("{pad}</{name}>\n"));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// docs/UIElements.md: a password field reports its value as a masked
    /// placeholder in the agent's view, never the contents. The same rule the
    /// paint path applies, kept here because the API key entered in Settings
    /// travels through exactly this view.
    #[test]
    fn passwords_are_masked_in_the_agent_view() {
        let tree = parse(
            r#"<window title="Settings">
                 <field id="api-key" kind="password" value="sk-ant-secret" description="The key"/>
                 <field id="name" value="plain" description="A name"/>
               </window>"#,
        )
        .unwrap();
        let view = agent_view(&tree, "awsettings", 1);
        assert!(!view.contains("sk-ant-secret"), "the secret leaked: {view}");
        assert!(view.contains("value=\"*************\""), "no mask: {view}");
        assert!(view.contains("value=\"plain\""), "a plain field kept its value: {view}");
    }
}
