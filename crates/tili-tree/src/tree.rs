use std::collections::HashMap;

use slotmap::{SlotMap, new_key_type};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Orientation {
    /// Children arranged side by side (varies x, splits width) — i3 calls
    /// this "splith".
    Horizontal,
    /// Children stacked top to bottom (varies y, splits height) — i3 calls
    /// this "splitv".
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

/// The container tree for one workspace: a binary tree of splits with
/// windows at the leaves. Pure data + algorithms — no macOS/AX types appear
/// anywhere in this crate, so all of this is unit-testable without a Mac.
#[derive(Debug, Default)]
pub struct Tree {
    nodes: SlotMap<NodeId, Node>,
    parents: HashMap<NodeId, NodeId>,
    root: Option<NodeId>,
}

impl Tree {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn root(&self) -> Option<NodeId> {
        self.root
    }

    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// A reasonable node to focus when this tree becomes the active
    /// workspace and nothing better (a remembered previous focus) is
    /// available — the root's first leaf, or `None` if the tree is empty.
    pub fn default_focus(&self) -> Option<NodeId> {
        self.root.map(|root| self.first_leaf(root))
    }

    pub fn window_at(&self, node: NodeId) -> Option<WindowId> {
        match self.nodes.get(node)? {
            Node::Window { window } => Some(*window),
            _ => None,
        }
    }

    /// Every window currently in the tree, in no particular order.
    pub fn window_ids(&self) -> Vec<WindowId> {
        self.nodes
            .values()
            .filter_map(|node| match node {
                Node::Window { window } => Some(*window),
                _ => None,
            })
            .collect()
    }

    /// Finds the leaf node for a window, if it's in the tree.
    pub fn find_node(&self, window: WindowId) -> Option<NodeId> {
        self.nodes.iter().find_map(|(id, node)| match node {
            Node::Window { window: w } if *w == window => Some(id),
            _ => None,
        })
    }

    /// Inserts a new window as a sibling of `near` (splitting it into a new
    /// 2-child `Split`), or as the sole root if the tree is empty. Returns
    /// the new leaf's id — callers typically focus it immediately.
    ///
    /// If `near` isn't a valid leaf in this tree (stale id, or a container
    /// id), falls back to the tree's root so the window still lands
    /// somewhere sensible rather than being dropped.
    pub fn insert_window(&mut self, window: WindowId, near: Option<NodeId>) -> NodeId {
        let new_leaf = self.nodes.insert(Node::Window { window });

        let Some(root) = self.root else {
            self.root = Some(new_leaf);
            return new_leaf;
        };

        let target = near.filter(|&n| self.nodes.contains_key(n)).unwrap_or(root);
        let target = self.first_leaf(target);
        self.split_leaf(target, new_leaf);
        new_leaf
    }

    fn split_leaf(&mut self, existing_leaf: NodeId, new_leaf: NodeId) {
        let parent = self.parents.get(&existing_leaf).copied();
        let new_split = self.nodes.insert(Node::Split {
            // New splits default to side-by-side. A smarter aspect-ratio
            // heuristic (wide area -> horizontal, tall area -> vertical) is
            // a reasonable future refinement, not required for M3.
            orientation: Orientation::Horizontal,
            children: vec![existing_leaf, new_leaf],
            ratios: vec![0.5, 0.5],
        });
        self.parents.insert(existing_leaf, new_split);
        self.parents.insert(new_leaf, new_split);

        match parent {
            Some(p) => {
                self.replace_child(p, existing_leaf, new_split);
                self.parents.insert(new_split, p);
            }
            None => self.root = Some(new_split),
        }
    }

    /// Removes a window from the tree, collapsing its now-single-child
    /// parent split if needed. Returns a reasonable node to focus next
    /// (a remaining sibling), or `None` if the tree is now empty.
    pub fn remove_window(&mut self, window: WindowId) -> Option<NodeId> {
        let leaf = self.find_node(window)?;
        let parent = self.parents.remove(&leaf);
        self.nodes.remove(leaf);

        let Some(parent_id) = parent else {
            self.root = None;
            return None;
        };

        let remaining_after_removal = {
            let Some(Node::Split {
                children, ratios, ..
            }) = self.nodes.get_mut(parent_id)
            else {
                return None;
            };
            let idx = children.iter().position(|&c| c == leaf)?;
            children.remove(idx);
            if idx < ratios.len() {
                ratios.remove(idx);
            }
            (children.len() == 1).then(|| children[0])
        };

        if let Some(remaining_child) = remaining_after_removal {
            let grandparent = self.parents.remove(&parent_id);
            self.nodes.remove(parent_id);
            match grandparent {
                Some(gp) => {
                    self.replace_child(gp, parent_id, remaining_child);
                    self.parents.insert(remaining_child, gp);
                }
                None => {
                    self.root = Some(remaining_child);
                    self.parents.remove(&remaining_child);
                }
            }
            Some(self.first_leaf(remaining_child))
        } else {
            let Some(Node::Split { children, .. }) = self.nodes.get(parent_id) else {
                return None;
            };
            children.first().map(|&c| self.first_leaf(c))
        }
    }

    /// Walks up from `from` to the nearest ancestor `Split` whose
    /// orientation matches `dir`'s axis and where `from` isn't already the
    /// boundary child, then descends into the neighboring subtree.
    /// Returns `None` if `from` is already at the tree's edge in `dir`.
    pub fn navigate(&self, from: NodeId, dir: Direction) -> Option<NodeId> {
        let axis = match dir {
            Direction::Left | Direction::Right => Orientation::Horizontal,
            Direction::Up | Direction::Down => Orientation::Vertical,
        };
        let forward = matches!(dir, Direction::Right | Direction::Down);

        let mut child = from;
        while let Some(&parent) = self.parents.get(&child) {
            if let Some(Node::Split {
                orientation,
                children,
                ..
            }) = self.nodes.get(parent)
                && *orientation == axis
            {
                let idx = children.iter().position(|&c| c == child)?;
                let target_idx = if forward {
                    idx.checked_add(1)
                } else {
                    idx.checked_sub(1)
                };
                if let Some(&target) = target_idx.and_then(|i| children.get(i)) {
                    return Some(self.first_leaf(target));
                }
            }
            child = parent;
        }
        None
    }

    /// Swaps the windows held by two leaf nodes in place — used for `move`,
    /// which (in this simplified implementation) swaps the focused window
    /// with whatever's adjacent rather than re-parenting it into the
    /// neighboring container.
    pub fn swap_windows(&mut self, a: NodeId, b: NodeId) {
        if a == b {
            return;
        }
        let (Some(wa), Some(wb)) = (self.window_at(a), self.window_at(b)) else {
            return;
        };
        if let Some(Node::Window { window }) = self.nodes.get_mut(a) {
            *window = wb;
        }
        if let Some(Node::Window { window }) = self.nodes.get_mut(b) {
            *window = wa;
        }
    }

    /// Computes each window's frame within `area` by recursively dividing
    /// it per the tree's splits.
    pub fn layout(&self, area: Rect) -> Vec<(WindowId, Rect)> {
        let mut out = Vec::new();
        if let Some(root) = self.root {
            self.layout_node(root, area, &mut out);
        }
        out
    }

    fn layout_node(&self, node: NodeId, area: Rect, out: &mut Vec<(WindowId, Rect)>) {
        match self.nodes.get(node) {
            Some(Node::Window { window }) => out.push((*window, area)),
            Some(Node::Split {
                orientation,
                children,
                ratios,
            }) => {
                let n = children.len();
                if n == 0 {
                    return;
                }
                let effective_ratios: Vec<f32> = if ratios.len() == n {
                    ratios.clone()
                } else {
                    vec![1.0 / n as f32; n]
                };
                let total: f32 = effective_ratios.iter().sum();
                let mut offset = 0.0_f64;
                for (i, &child) in children.iter().enumerate() {
                    let fraction = f64::from(effective_ratios[i] / total);
                    let child_area = match orientation {
                        Orientation::Horizontal => Rect {
                            x: area.x + offset,
                            y: area.y,
                            width: area.width * fraction,
                            height: area.height,
                        },
                        Orientation::Vertical => Rect {
                            x: area.x,
                            y: area.y + offset,
                            width: area.width,
                            height: area.height * fraction,
                        },
                    };
                    offset += match orientation {
                        Orientation::Horizontal => child_area.width,
                        Orientation::Vertical => child_area.height,
                    };
                    self.layout_node(child, child_area, out);
                }
            }
            // Accordion layout algorithm proper lands in M7; until then,
            // just show whichever child is marked active so an Accordion
            // node in the tree doesn't break layout.
            Some(Node::Accordion { children, active }) => {
                if let Some(&active_child) = children.get(*active) {
                    self.layout_node(active_child, area, out);
                }
            }
            None => {}
        }
    }

    fn replace_child(&mut self, parent: NodeId, old: NodeId, new: NodeId) {
        if let Some(Node::Split { children, .. } | Node::Accordion { children, .. }) =
            self.nodes.get_mut(parent)
            && let Some(pos) = children.iter().position(|&c| c == old)
        {
            children[pos] = new;
        }
    }

    fn first_leaf(&self, mut node: NodeId) -> NodeId {
        loop {
            match self.nodes.get(node) {
                Some(Node::Split { children, .. } | Node::Accordion { children, .. }) => {
                    match children.first() {
                        Some(&first) => node = first,
                        None => return node,
                    }
                }
                _ => return node,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect {
            x: 0.0,
            y: 0.0,
            width: 1000.0,
            height: 800.0,
        }
    }

    #[test]
    fn empty_tree_has_no_root() {
        let tree = Tree::new();
        assert_eq!(tree.root(), None);
        assert!(tree.is_empty());
        assert!(tree.layout(area()).is_empty());
    }

    #[test]
    fn first_window_becomes_root() {
        let mut tree = Tree::new();
        let leaf = tree.insert_window(1, None);
        assert_eq!(tree.root(), Some(leaf));
        assert_eq!(tree.window_at(leaf), Some(1));

        let layout = tree.layout(area());
        assert_eq!(layout, vec![(1, area())]);
    }

    #[test]
    fn second_window_splits_side_by_side_and_fills_area() {
        let mut tree = Tree::new();
        let first = tree.insert_window(1, None);
        tree.insert_window(2, Some(first));

        let layout = tree.layout(area());
        assert_eq!(layout.len(), 2);

        let total_width: f64 = layout.iter().map(|(_, r)| r.width).sum();
        assert!((total_width - area().width).abs() < f64::EPSILON);
        for (_, rect) in &layout {
            assert_eq!(rect.height, area().height);
            assert_eq!(rect.y, 0.0);
        }
    }

    #[test]
    fn navigate_left_right_across_a_horizontal_split() {
        let mut tree = Tree::new();
        let left = tree.insert_window(1, None);
        let right_leaf = tree.insert_window(2, Some(left));

        assert_eq!(tree.navigate(left, Direction::Right), Some(right_leaf));
        assert_eq!(tree.navigate(right_leaf, Direction::Left), Some(left));
        // No window above/below in a purely horizontal split.
        assert_eq!(tree.navigate(left, Direction::Up), None);
        assert_eq!(tree.navigate(left, Direction::Down), None);
        // Already at the left edge.
        assert_eq!(tree.navigate(left, Direction::Left), None);
    }

    #[test]
    fn move_swaps_window_identities_between_leaves() {
        let mut tree = Tree::new();
        let a = tree.insert_window(1, None);
        let b = tree.insert_window(2, Some(a));

        tree.swap_windows(a, b);
        assert_eq!(tree.window_at(a), Some(2));
        assert_eq!(tree.window_at(b), Some(1));
    }

    #[test]
    fn remove_collapses_parent_split_and_leaves_sibling_as_root() {
        let mut tree = Tree::new();
        let a = tree.insert_window(1, None);
        tree.insert_window(2, Some(a));

        let next_focus = tree.remove_window(1);
        assert_eq!(tree.window_ids(), vec![2]);
        assert_eq!(tree.window_at(next_focus.unwrap()), Some(2));
        // Root itself should now be the surviving window's own leaf, not a
        // dangling single-child split.
        assert_eq!(tree.window_at(tree.root().unwrap()), Some(2));

        let layout = tree.layout(area());
        assert_eq!(layout, vec![(2, area())]);
    }

    #[test]
    fn remove_last_window_empties_the_tree() {
        let mut tree = Tree::new();
        tree.insert_window(1, None);
        let next_focus = tree.remove_window(1);
        assert_eq!(next_focus, None);
        assert!(tree.is_empty());
    }

    #[test]
    fn three_windows_navigate_correctly() {
        // 1 | 2 | 3, all in one horizontal split (each insert splits the
        // previously-focused leaf, so inserting 3 next to 2 nests it under 2
        // rather than flattening into a 3-way split — still a valid tree
        // shape for navigation purposes).
        let mut tree = Tree::new();
        let w1 = tree.insert_window(1, None);
        let w2 = tree.insert_window(2, Some(w1));
        let w3 = tree.insert_window(3, Some(w2));

        assert_eq!(tree.navigate(w1, Direction::Right), Some(w2));
        assert_eq!(tree.navigate(w2, Direction::Right), Some(w3));
        assert_eq!(tree.navigate(w3, Direction::Left), Some(w2));
        assert_eq!(tree.navigate(w2, Direction::Left), Some(w1));
        assert_eq!(tree.navigate(w3, Direction::Right), None);
        assert_eq!(tree.navigate(w1, Direction::Left), None);
    }
}
