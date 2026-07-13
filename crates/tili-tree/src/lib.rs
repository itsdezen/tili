use slotmap::{new_key_type, SlotMap};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

new_key_type! { pub struct NodeId; }

/// A single window's identity, matching the real macOS `CGWindowID`.
/// Resolved once in `tili-ax` and treated as an opaque key everywhere else.
pub type WindowId = u32;

#[derive(Debug, Clone)]
pub enum Node {
    Split {
        orientation: Orientation,
        children: Vec<NodeId>,
        ratios: Vec<f32>,
    },
    Accordion {
        children: Vec<NodeId>,
        active: usize,
    },
    Window {
        window: WindowId,
    },
}

#[derive(Debug, Default)]
pub struct Tree {
    nodes: SlotMap<NodeId, Node>,
    parents: std::collections::HashMap<NodeId, NodeId>,
    root: Option<NodeId>,
}

impl Tree {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn root(&self) -> Option<NodeId> {
        self.root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_tree_has_no_root() {
        let tree = Tree::new();
        assert_eq!(tree.root(), None);
    }
}
