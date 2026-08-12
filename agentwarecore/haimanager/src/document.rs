//! A parsed tree with a version on it, and the diff between two of them.
//!
//! Applications resend their whole interface on every change, so the compositor
//! sees a stream of complete documents rather than a stream of edits. Two
//! questions have to be answered each time one arrives, and both are answered
//! here.
//!
//! **Which old node is this new node?** Ephemeral state lives in the compositor
//! rather than in the tree: focus, the caret inside a text field, scroll
//! offsets. Carrying it across a re-render means matching old node to new node,
//! and the only thing that can do that is an identity the application supplies.
//! That is what makes stable `id`s load-bearing rather than a nicety: an app
//! that regenerates ids per frame silently moves the human's cursor on every
//! keystroke and moves the agent's target out from under it between reading a
//! screen and acting on it.
//!
//! Not every node has an id, and requiring one everywhere would mean apps
//! writing meaningless identifiers for layout scaffolding. Anything without one
//! gets a **structural key**: its position among its siblings, under its
//! parent's key. That is exactly as stable as the shape of the document, which
//! is the right amount for the things that need it, since the nodes without ids
//! are the ones an agent cannot address either.
//!
//! **Did anything actually change?** A resend that is byte-for-byte the state
//! already on screen should cost nothing. An application in a polling loop, or
//! one that resends after a keystroke it turned out not to care about, is
//! ordinary and should not repaint the screen.
//!
//! The diff is deliberately a summary rather than a patch list. Painting is a
//! full canvas pass today, so per-node damage would buy nothing measurable and
//! would have to be kept correct against every drawing change. What the summary
//! is used for is real: skip the frame entirely when it is empty.

use std::collections::HashMap;

use crate::awml::{self, Tree};

pub struct Document {
    pub tree: Tree,
    /// The version the client stamped on this tree. Echoed back on every event
    /// generated against it, so the client can discard one it has moved past.
    pub version: u64,
    /// One identity per node, indexed as the arena is.
    keys: Vec<String>,
}

impl Document {
    pub fn parse(source: &str, version: u64) -> Result<Document, awml::Error> {
        let tree = awml::parse(source)?;
        let keys = compute_keys(&tree);
        Ok(Document { tree, version, keys })
    }

    pub fn key(&self, index: usize) -> &str {
        &self.keys[index]
    }

    pub fn index_of(&self, key: &str) -> Option<usize> {
        self.keys.iter().position(|candidate| candidate == key)
    }

    pub fn has_key(&self, key: &str) -> bool {
        self.keys.iter().any(|candidate| candidate == key)
    }

    /// Compare this document against the one currently held.
    pub fn diff(&self, previous: &Document) -> Diff {
        let old: HashMap<&str, usize> = previous
            .keys
            .iter()
            .enumerate()
            .map(|(index, key)| (key.as_str(), index))
            .collect();

        let mut diff = Diff::default();

        for (index, key) in self.keys.iter().enumerate() {
            let Some(&was) = old.get(key.as_str()) else {
                diff.added += 1;
                continue;
            };
            // A node that kept its identity but changed position is not a
            // content change, but it does invalidate the layout: two buttons
            // swapping places renders differently while every node compares
            // equal.
            if was != index {
                diff.moved += 1;
            }
            if !same(previous, was, self, index) {
                diff.changed += 1;
            }
        }

        diff.removed = previous
            .keys
            .iter()
            .filter(|key| !self.has_key(key))
            .count();

        diff
    }
}

#[derive(Default, Clone, Copy)]
pub struct Diff {
    pub added: usize,
    pub removed: usize,
    pub moved: usize,
    pub changed: usize,
}

impl Diff {
    /// True when the new tree renders exactly as the held one does.
    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.removed == 0 && self.moved == 0 && self.changed == 0
    }

    pub fn summary(&self) -> String {
        format!(
            "+{} -{} ~{} moved {}",
            self.added, self.removed, self.changed, self.moved
        )
    }
}

/// Whether two nodes would draw identically.
///
/// Attributes are compared as a set rather than as a sequence: an application
/// that reorders its attributes between renders has changed nothing, and
/// reporting that as a change would repaint the screen for no reason.
fn same(old: &Document, a: usize, new: &Document, b: usize) -> bool {
    let (first, second) = (old.tree.node(a), new.tree.node(b));

    first.tag == second.tag
        && first.text == second.text
        && first.children.len() == second.children.len()
        && first.attrs.len() == second.attrs.len()
        && second
            .attrs
            .iter()
            .all(|(key, value)| first.attr(key) == Some(value.as_str()))
}

fn compute_keys(tree: &Tree) -> Vec<String> {
    let mut keys = vec![String::new(); tree.nodes.len()];
    assign(tree, Tree::ROOT, "", 0, &mut keys);
    keys
}

/// An id beats position, everywhere it exists.
///
/// A node with an id keeps its identity wherever the application moves it, which
/// is what an application promises by giving it one. A node without one is
/// identified by where it sits, so its key changes if the shape around it does,
/// which is the honest answer: there is nothing else to go on.
fn assign(tree: &Tree, index: usize, parent: &str, position: usize, keys: &mut Vec<String>) {
    let node = tree.node(index);

    let key = match node.id() {
        Some(id) => format!("#{id}"),
        None => format!("{parent}/{}{position}", node.tag.name()),
    };

    for (at, &child) in node.children.iter().enumerate() {
        assign(tree, child, &key, at, keys);
    }
    keys[index] = key;
}
