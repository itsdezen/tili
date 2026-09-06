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

/// A resize-able border found by `resize_handle_at`, between two children of one specific
/// `Tiles` container. `before`/`after` are that container's own direct children — either could
/// themselves be a sub-container, not necessarily the window whose edge was actually dragged —
/// which is what makes it safe to pass either one straight to `resize_weight`: its own upward
/// walk to the nearest `Tiles` ancestor matches on the very first step, since `before`/`after`
/// are already direct children of the container that walk would find anyway.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResizeHandle {
    pub before: NodeId,
    pub after: NodeId,
    pub orientation: Orientation,
    /// Weight-units one pixel along this container's split axis is worth right now — i.e.
    /// `total sibling weight / divisible pixels` for this specific container. Constant for as
    /// long as this container's own on-screen area and total weight don't change, which holds
    /// for the whole duration of a single mouse drag (a resize only redistributes weight among
    /// siblings — their sum never changes — and the monitor doesn't move mid-drag).
    pub weight_per_pixel: f64,
}

/// Which of a container's two rendering modes is active — orthogonal to
/// `Orientation` (a container has both, independently: layout and
/// orientation vary separately).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Layout {
    #[default]
    Tiles,
    Accordion,
}

/// Spacing applied during layout — config-driven from M5 on. `outer` is
/// CSS-shorthand ordered: (top, right, bottom, left). `accordion` is how
/// much of each non-focused `Accordion` sibling peeks out from behind the
/// active one, on the side(s) where a sibling exists — `0.0` collapses
/// every child to the exact same full frame.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Gaps {
    pub inner: f64,
    pub outer: (f64, f64, f64, f64),
    /// Overrides `outer` when a workspace has exactly one tile (floating
    /// windows don't count). `None` falls back to `outer`.
    pub outer_solo: Option<(f64, f64, f64, f64)>,
    pub accordion: f64,
}

new_key_type! { pub struct NodeId; }

/// A single window's identity, matching the real macOS `CGWindowID`.
/// Resolved once in `tili-ax` and treated as an opaque key everywhere else.
pub type WindowId = u32;

/// A container holds an arbitrary number of children (windows and/or
/// nested containers) — not a binary tree. `weights` are per-child
/// proportions consumed by `Tiles` layout (arbitrary positive numbers,
/// normalized by their sum — not required to add up to any particular
/// total). `mru` ("most recently used") is the index of whichever child
/// was most recently focused/moved-to: it doubles as both "which child is
/// visible" for `Accordion` and "which child navigation descends into" for
/// `Tiles`, unifying the two concepts rather than tracking them separately.
#[derive(Debug, Clone)]
pub enum Node {
    Container {
        layout: Layout,
        orientation: Orientation,
        children: Vec<NodeId>,
        weights: Vec<f32>,
        mru: usize,
    },
    Window {
        window: WindowId,
    },
    /// A floating window's focus/topology placeholder: a normal child for
    /// insertion/removal/lookup/`mru` purposes (so `workspace_focus` can
    /// address it exactly like a `Window` leaf), but excluded from every
    /// `Tiles`/`Accordion` sizing computation in `layout` — a floating
    /// window's real position/size is owned by `tili-daemon`'s
    /// `placements`/`compute_floating_frame`, not by this tree. Still
    /// carries a `weights` entry (kept index-parallel with `children`, like
    /// every other child) even though `layout` never reads it.
    Floating {
        window: WindowId,
    },
}

/// The container tree for one workspace: an n-ary tree of containers with
/// windows at the leaves. Pure data + algorithms — no macOS/AX types appear
/// anywhere in this crate, so all of this is unit-testable without a Mac.
///
/// Two invariants hold after every mutating call (enforced by `normalize`,
/// run at the end of every one of them): no container has exactly one
/// child (it gets flattened away — the root is the one place this
/// "flattening" bottoms out as a bare `Window` leaf with no wrapping
/// container at all, rather than disappearing entirely), and no `Tiles`
/// container is directly nested inside a same-orientation `Tiles` parent
/// (the child's children get spliced into the parent instead). A `Window`
/// leaf's `NodeId` never changes once inserted — only containers are
/// created/destroyed — so callers (`tili-daemon`'s `workspace_focus`) can
/// hold onto a leaf's `NodeId` across arbitrary tree mutations.
#[derive(Debug, Default)]
pub struct Tree {
    nodes: SlotMap<NodeId, Node>,
    parents: HashMap<NodeId, NodeId>,
    root: Option<NodeId>,
    /// Layout a workspace's very first container is created with — set
    /// from config before any window lands, read once by `insert_leaf`.
    /// Has no effect once a container already exists.
    default_layout: Layout,
}

/// `tiles_layout_inputs`'s return shape: sizeable children with their
/// weights, total weight, and divisible space.
type TilesLayoutInputs = (Vec<(NodeId, f32)>, f32, f64);

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

    pub fn set_default_layout(&mut self, layout: Layout) {
        self.default_layout = layout;
    }

    /// A reasonable node to focus when this tree becomes the active
    /// workspace and nothing better (a remembered previous focus) is
    /// available — the root's MRU leaf, or `None` if the tree is empty.
    pub fn default_focus(&self) -> Option<NodeId> {
        self.root.map(|root| self.mru_leaf(root))
    }

    pub fn window_at(&self, node: NodeId) -> Option<WindowId> {
        match self.nodes.get(node)? {
            Node::Window { window } | Node::Floating { window } => Some(*window),
            Node::Container { .. } => None,
        }
    }

    /// Reverse of `window_at` — which node holds a given window, if any.
    /// O(n) in this tree's node count, but only ever called in response to a
    /// real OS focus-change event (see `WmState::sync_focus_from_pid`), not
    /// on any hot path.
    pub fn node_for_window(&self, window: WindowId) -> Option<NodeId> {
        self.nodes.iter().find_map(|(id, node)| match node {
            Node::Window { window: w } | Node::Floating { window: w } if *w == window => Some(id),
            _ => None,
        })
    }

    /// Every window currently in the tree, tiled or floating, in no
    /// particular order.
    pub fn window_ids(&self) -> Vec<WindowId> {
        self.nodes
            .values()
            .filter_map(|node| match node {
                Node::Window { window } | Node::Floating { window } => Some(*window),
                Node::Container { .. } => None,
            })
            .collect()
    }

    /// Just the *tiled* windows currently in the tree — `window_ids` minus
    /// any `Floating` leaves. Callers that need to lay out or park tiled
    /// windows specifically (not just look one up) want this, not
    /// `window_ids`.
    pub fn tiled_window_ids(&self) -> Vec<WindowId> {
        self.nodes
            .values()
            .filter_map(|node| match node {
                Node::Window { window } => Some(*window),
                Node::Container { .. } | Node::Floating { .. } => None,
            })
            .collect()
    }

    /// Finds the leaf node for a window, if it's in the tree. Same lookup as
    /// `node_for_window` (the two names exist for different callers' reading
    /// context), so this just delegates to it.
    pub fn find_node(&self, window: WindowId) -> Option<NodeId> {
        self.node_for_window(window)
    }

    /// Inserts a new window as a sibling of `near`, in `near`'s own parent
    /// container (i3-style flat insertion — never wraps `near` in a fresh
    /// 2-child container the way a binary-tree insert would), or as
    /// the sole root if the tree is empty. `root_orientation` only matters
    /// the one time this call is what turns a lone-window root into an
    /// actual container (the tree's second-ever window) — every other call
    /// inserts into an already-oriented container and ignores it. Returns
    /// the new leaf's id — callers typically focus it immediately.
    ///
    /// If `near` isn't a valid leaf in this tree (stale id, or a container
    /// id), falls back to the tree's MRU leaf so the window still lands
    /// somewhere sensible rather than being dropped.
    pub fn insert_window(
        &mut self,
        window: WindowId,
        near: Option<NodeId>,
        root_orientation: Orientation,
    ) -> NodeId {
        self.insert_leaf(Node::Window { window }, near, root_orientation)
    }

    /// Same sibling-insertion placement as `insert_window`, but for a
    /// `Node::Floating` leaf — gives a floating window a `NodeId` so
    /// `workspace_focus`/`focused_node()` can address it like any tiled
    /// window, without it ever affecting `layout`'s sizing (see `Node`'s
    /// `Floating` doc comment).
    pub fn insert_floating(
        &mut self,
        window: WindowId,
        near: Option<NodeId>,
        root_orientation: Orientation,
    ) -> NodeId {
        self.insert_leaf(Node::Floating { window }, near, root_orientation)
    }

    fn insert_leaf(
        &mut self,
        leaf: Node,
        near: Option<NodeId>,
        root_orientation: Orientation,
    ) -> NodeId {
        let new_leaf = self.nodes.insert(leaf);

        let Some(root) = self.root else {
            self.root = Some(new_leaf);
            return new_leaf;
        };

        let target = near.filter(|&n| self.nodes.contains_key(n)).unwrap_or(root);
        let target = self.mru_leaf(target);

        match self.parents.get(&target).copied() {
            Some(parent) => {
                if let Some(Node::Container {
                    children,
                    weights,
                    mru,
                    ..
                }) = self.nodes.get_mut(parent)
                {
                    let idx = children
                        .iter()
                        .position(|&c| c == target)
                        .unwrap_or(children.len().saturating_sub(1));
                    let avg_weight = if weights.is_empty() {
                        1.0
                    } else {
                        weights.iter().sum::<f32>() / weights.len() as f32
                    };
                    children.insert(idx + 1, new_leaf);
                    weights.insert(idx + 1, avg_weight);
                    *mru = idx + 1;
                }
                self.parents.insert(new_leaf, parent);
            }
            None => {
                // `target` (== root) has no parent container yet — it's a
                // lone window root. This insert is what creates the very
                // first container.
                let new_root = self.nodes.insert(Node::Container {
                    layout: self.default_layout,
                    orientation: root_orientation,
                    children: vec![target, new_leaf],
                    weights: vec![1.0, 1.0],
                    mru: 1,
                });
                self.parents.insert(target, new_root);
                self.parents.insert(new_leaf, new_root);
                self.root = Some(new_root);
            }
        }

        self.normalize();
        new_leaf
    }

    /// Removes a window from the tree, collapsing any now-empty or
    /// now-single-child ancestor containers. Returns a reasonable node to
    /// focus next (the MRU leaf of whatever's left near where the removed
    /// window was), or `None` if the tree is now empty.
    pub fn remove_window(&mut self, window: WindowId) -> Option<NodeId> {
        let leaf = self.find_node(window)?;
        let Some(mut container_id) = self.parents.remove(&leaf) else {
            // The removed leaf was the lone root window.
            self.nodes.remove(leaf);
            self.root = None;
            return None;
        };
        self.nodes.remove(leaf);
        self.remove_child(container_id, leaf);

        // Walk up removing now-empty containers (a container that drops to
        // exactly one child is left for `normalize`'s flatten pass, since
        // it's still a well-formed — if soon-to-be-flattened — tree).
        loop {
            let is_empty = matches!(self.nodes.get(container_id), Some(Node::Container { children, .. }) if children.is_empty());
            if !is_empty {
                break;
            }
            let grandparent = self.parents.remove(&container_id);
            self.nodes.remove(container_id);
            match grandparent {
                Some(gp) => {
                    self.remove_child(gp, container_id);
                    container_id = gp;
                }
                None => {
                    self.root = None;
                    return None;
                }
            }
        }

        let focus = self.mru_leaf(container_id);
        self.normalize();
        Some(focus)
    }

    /// Removes `child` from `container`'s children/weights, keeping `mru`
    /// pointing at the same surviving child it did before removal (not
    /// whatever now sits at its old index). Does not touch `child` itself
    /// or `self.parents[child]` — callers that are relocating (not
    /// deleting) `child` handle that separately.
    fn remove_child(&mut self, container: NodeId, child: NodeId) {
        let Some(Node::Container {
            children,
            weights,
            mru,
            ..
        }) = self.nodes.get_mut(container)
        else {
            return;
        };
        let Some(idx) = children.iter().position(|&c| c == child) else {
            return;
        };
        children.remove(idx);
        if idx < weights.len() {
            weights.remove(idx);
        }
        if !children.is_empty() {
            if idx < *mru {
                *mru -= 1;
            } else if *mru >= children.len() {
                *mru = children.len() - 1;
            }
        }
    }

    /// Detaches `node` from `parent`'s children (bookkeeping only — `node`
    /// stays alive, just orphaned) so it can be re-inserted elsewhere.
    fn detach(&mut self, node: NodeId, parent: NodeId) {
        self.parents.remove(&node);
        self.remove_child(parent, node);
    }

    /// Inserts `node` as `anchor`'s new sibling, immediately before it if
    /// `before` else immediately after, taking on `anchor`'s current weight
    /// (split — this is a fresh landing spot, not a resize) and becoming
    /// the container's new MRU child.
    fn insert_sibling(&mut self, node: NodeId, anchor: NodeId, before: bool) {
        let Some(&parent) = self.parents.get(&anchor) else {
            return;
        };
        let Some(Node::Container {
            children,
            weights,
            mru,
            ..
        }) = self.nodes.get_mut(parent)
        else {
            return;
        };
        let Some(anchor_idx) = children.iter().position(|&c| c == anchor) else {
            return;
        };
        let insert_idx = if before { anchor_idx } else { anchor_idx + 1 };
        let weight = weights.get(anchor_idx).copied().unwrap_or(1.0);
        children.insert(insert_idx, node);
        weights.insert(insert_idx, weight);
        *mru = insert_idx;
        self.parents.insert(node, parent);
    }

    /// Records that `node` was just focused: walks from `node` to the root,
    /// setting each ancestor container's `mru` to the branch leading to
    /// `node`. This is what makes `Accordion` show the right child and
    /// `Tiles` navigation descend to where the user actually was, rather
    /// than always landing on a container's first child.
    pub fn record_focus(&mut self, node: NodeId) {
        let mut child = node;
        while let Some(&parent) = self.parents.get(&child) {
            if let Some(Node::Container { children, mru, .. }) = self.nodes.get_mut(parent)
                && let Some(idx) = children.iter().position(|&c| c == child)
            {
                *mru = idx;
            }
            child = parent;
        }
    }

    /// Whether `node` (if any) resolves to an `Accordion` container — the
    /// shared predicate behind `is_accordion_container`/`is_root_accordion`.
    fn is_accordion(&self, node: Option<NodeId>) -> bool {
        node.and_then(|n| self.nodes.get(n)).is_some_and(|n| {
            matches!(
                n,
                Node::Container {
                    layout: Layout::Accordion,
                    ..
                }
            )
        })
    }

    /// Whether `from`'s immediate parent container is an `Accordion` (as
    /// opposed to `Tiles`, or `from` having no parent at all).
    pub fn is_accordion_container(&self, from: NodeId) -> bool {
        self.is_accordion(self.parents.get(&from).copied())
    }

    /// Whether the workspace's root container is itself an `Accordion` —
    /// the root-container analogue of `is_accordion_container`, used by
    /// `layout --root` (`set_layout`) since the root has no parent to check.
    pub fn is_root_accordion(&self) -> bool {
        self.is_accordion(self.root)
    }

    /// `from`'s immediate parent container's orientation, or `None` if
    /// `from` has no parent (the lone root window).
    pub fn orientation_of(&self, from: NodeId) -> Option<Orientation> {
        let &parent = self.parents.get(&from)?;
        match self.nodes.get(parent)? {
            Node::Container { orientation, .. } => Some(*orientation),
            Node::Window { .. } | Node::Floating { .. } => None,
        }
    }

    /// The workspace root container's orientation, or `None` if the tree is
    /// empty or the root is a lone window (tiled or floating) with no
    /// container.
    pub fn root_orientation(&self) -> Option<Orientation> {
        match self.nodes.get(self.root?)? {
            Node::Container { orientation, .. } => Some(*orientation),
            Node::Window { .. } | Node::Floating { .. } => None,
        }
    }

    /// Converts `from`'s immediate parent container between `Tiles` and
    /// `Accordion` in place — orientation, children, weights, and MRU are
    /// all shared fields, so this is a pure flip of `layout`, nothing else
    /// changes (the currently-visible/focused child stays exactly that,
    /// since `mru` already tracks it independently of `layout`).
    ///
    /// Returns `false` (nothing changed) if `from` has no parent — a lone
    /// root window has no container to toggle.
    pub fn toggle_layout(&mut self, from: NodeId) -> bool {
        let Some(&parent) = self.parents.get(&from) else {
            return false;
        };
        self.flip_layout(parent)
    }

    /// Same conversion as `toggle_layout`, but always targets the
    /// workspace's root container. Returns `false` if the tree is empty or
    /// the root itself is a lone window (nothing to toggle).
    pub fn toggle_root_layout(&mut self) -> bool {
        let Some(root) = self.root else {
            return false;
        };
        self.flip_layout(root)
    }

    fn flip_layout(&mut self, container: NodeId) -> bool {
        match self.nodes.get_mut(container) {
            Some(Node::Container { layout, .. }) => {
                *layout = match layout {
                    Layout::Tiles => Layout::Accordion,
                    Layout::Accordion => Layout::Tiles,
                };
                true
            }
            _ => false,
        }
    }

    /// Sets `from`'s immediate parent container's orientation. A no-op
    /// (still returns `true`) if it's already that orientation — there are
    /// only two, so "set" and "flip away from the other one" coincide.
    /// Returns `false` if `from` has no parent container.
    pub fn set_orientation(&mut self, from: NodeId, orientation: Orientation) -> bool {
        let Some(&parent) = self.parents.get(&from) else {
            return false;
        };
        self.apply_orientation(parent, orientation)
    }

    /// Root-container analogue of `set_orientation`, applying
    /// `horizontal`/`vertical` to the workspace root instead.
    pub fn set_root_orientation(&mut self, orientation: Orientation) -> bool {
        let Some(root) = self.root else {
            return false;
        };
        self.apply_orientation(root, orientation)
    }

    fn apply_orientation(&mut self, container: NodeId, orientation: Orientation) -> bool {
        let Some(Node::Container { orientation: o, .. }) = self.nodes.get_mut(container) else {
            return false;
        };
        *o = orientation;
        // Changing orientation can newly agree with a same-orientation
        // parent (or child), which normalize's merge pass needs to clean up.
        self.normalize();
        true
    }

    /// Wraps `from` and its neighbor in `dir` (within `from`'s own parent
    /// container — this doesn't escalate up the tree the way `move` does)
    /// into a brand-new container, orientation perpendicular to `dir`
    /// (preferred over i3-style `split`, since `split` on its own would
    /// create a 1-child container that `normalize` would immediately
    /// flatten right back away). Returns `false` if `from`'s parent
    /// orientation doesn't match `dir`'s axis or there's no neighbor there.
    pub fn join_with(&mut self, from: NodeId, dir: Direction) -> bool {
        let axis = axis_for(dir);
        let forward = matches!(dir, Direction::Right | Direction::Down);
        let Some(&parent) = self.parents.get(&from) else {
            return false;
        };
        let Some(Node::Container {
            orientation,
            children,
            weights,
            ..
        }) = self.nodes.get(parent)
        else {
            return false;
        };
        if *orientation != axis {
            return false;
        }
        let Some(idx) = children.iter().position(|&c| c == from) else {
            return false;
        };
        let target_idx = if forward {
            idx.checked_add(1)
        } else {
            idx.checked_sub(1)
        };
        let Some(target_idx) = target_idx.filter(|&i| i < children.len()) else {
            return false;
        };
        let neighbor = children[target_idx];
        let combined_weight = weights[idx] + weights[target_idx];
        let insert_at = idx.min(target_idx);

        self.detach(from, parent);
        self.detach(neighbor, parent);

        let perpendicular = match axis {
            Orientation::Horizontal => Orientation::Vertical,
            Orientation::Vertical => Orientation::Horizontal,
        };
        let new_container = self.nodes.insert(Node::Container {
            layout: Layout::Tiles,
            orientation: perpendicular,
            children: vec![from, neighbor],
            weights: vec![1.0, 1.0],
            mru: 0,
        });
        self.parents.insert(from, new_container);
        self.parents.insert(neighbor, new_container);

        if let Some(Node::Container {
            children,
            weights,
            mru,
            ..
        }) = self.nodes.get_mut(parent)
        {
            let at = insert_at.min(children.len());
            children.insert(at, new_container);
            weights.insert(at, combined_weight);
            *mru = at;
        }
        self.parents.insert(new_container, parent);

        self.normalize();
        true
    }

    /// Grows (or, with a negative `delta`, shrinks) the weight of the
    /// branch containing `from` within its nearest ancestor `Tiles`
    /// container (any orientation — resize affects whichever axis that
    /// container actually splits on; an `Accordion` ancestor is skipped
    /// over since its children aren't sized against each other). The
    /// change is taken from (or given back to) the other siblings in that
    /// container, proportionally, clamped so no sibling's weight goes
    /// below a small minimum. `delta` is in the same weight-space as
    /// `Node::Container::weights` itself (not pixels) — small values like
    /// `0.05`-`0.2` are reasonable steps. Returns `false` if `from` has no
    /// `Tiles` ancestor with another child to trade weight with.
    pub fn resize_weight(&mut self, from: NodeId, delta: f32) -> bool {
        let mut child = from;
        while let Some(&parent) = self.parents.get(&child) {
            if let Some(Node::Container {
                layout: Layout::Tiles,
                ..
            }) = self.nodes.get(parent)
                && self.apply_resize(parent, child, delta)
            {
                return true;
            }
            child = parent;
        }
        false
    }

    fn apply_resize(&mut self, container: NodeId, branch: NodeId, delta: f32) -> bool {
        let Some((max_shrink, max_grow)) = self.branch_resize_bounds(container, branch) else {
            return false;
        };
        let actual_delta = delta.clamp(-max_shrink, max_grow);

        let Some(Node::Container {
            children, weights, ..
        }) = self.nodes.get_mut(container)
        else {
            return false;
        };
        let Some(idx) = children.iter().position(|&c| c == branch) else {
            return false;
        };
        let (others, others_total) = other_indices_and_total(children, weights, idx);

        weights[idx] += actual_delta;
        for &i in &others {
            weights[i] -= actual_delta * (weights[i] / others_total);
        }
        true
    }

    /// The `(max_shrink, max_grow)` a call to `apply_resize(container, branch, delta)` would
    /// actually honor right now, both `>= 0.0` — the exact clamp bound it applies internally,
    /// factored out so callers can pick a `delta` that's already valid instead of getting a
    /// silently-truncated one back. `None` if `branch` isn't a child of `container`, `container`
    /// has fewer than 2 children, or the other children's weights don't sum to a positive number.
    fn branch_resize_bounds(&self, container: NodeId, branch: NodeId) -> Option<(f32, f32)> {
        const MIN_WEIGHT: f32 = 0.05;
        let Some(Node::Container {
            children, weights, ..
        }) = self.nodes.get(container)
        else {
            return None;
        };
        if children.len() < 2 {
            return None;
        }
        let idx = children.iter().position(|&c| c == branch)?;
        let (others, others_total) = other_indices_and_total(children, weights, idx);
        if others_total <= 0.0 {
            return None;
        }
        // `apply_resize` shrinks every sibling by the same *proportional*
        // factor (each loses `delta * weights[i] / others_total`, i.e. every
        // sibling keeps the same fraction `1 - delta/others_total` of its
        // own weight) — so the smallest sibling is still the smallest after
        // shrinking, and it's the one that hits `MIN_WEIGHT` first. Bounding
        // `max_grow` off the *aggregate* (`others_total - MIN_WEIGHT * n`)
        // assumes an even split across siblings and doesn't hold when
        // weights are uneven: it can let a small sibling get crushed to a
        // sliver while the aggregate still looks fine. Solving
        // `min_weight * (1 - delta/others_total) >= MIN_WEIGHT` for `delta`
        // gives the bound below instead.
        let min_other_weight = others
            .iter()
            .map(|&i| weights[i])
            .fold(f32::INFINITY, f32::min);
        let max_grow = if min_other_weight <= MIN_WEIGHT {
            0.0
        } else {
            (others_total * (1.0 - MIN_WEIGHT / min_other_weight)).max(0.0)
        };
        let max_shrink = (weights[idx] - MIN_WEIGHT).max(0.0);
        Some((max_shrink, max_grow))
    }

    /// The `(max_shrink, max_grow)` bound `resize_weight(from, ±delta)` would actually honor
    /// right now — the same nearest-`Tiles`-ancestor walk as `resize_weight` itself, but
    /// read-only. Lets a caller (mouse-drag step-snapping, in particular) pick a `delta` that's
    /// already guaranteed valid instead of clamping into an arbitrary, possibly off-grid value
    /// after the fact. `None` if `from` has no `Tiles` ancestor with a sibling to trade weight
    /// with, mirroring `resize_weight`'s own `false`.
    pub fn resize_delta_bounds(&self, from: NodeId) -> Option<(f32, f32)> {
        let mut child = from;
        while let Some(&parent) = self.parents.get(&child) {
            if let Some(Node::Container {
                layout: Layout::Tiles,
                ..
            }) = self.nodes.get(parent)
                && let Some(bounds) = self.branch_resize_bounds(parent, child)
            {
                return Some(bounds);
            }
            child = parent;
        }
        None
    }

    /// Resets every child weight of the target container back to `1.0`,
    /// undoing any manual `resize_weight` calls. Same dual-target split as
    /// `set_orientation`/`set_root_orientation`: `root` targets the
    /// workspace's root container directly, otherwise it's `from`'s
    /// immediate parent.
    /// Returns `false` if there's no such container (a lone root window
    /// either way, or an empty tree when `root` is set).
    pub fn balance_weights(&mut self, from: NodeId, root: bool) -> bool {
        let target = if root {
            self.root
        } else {
            self.parents.get(&from).copied()
        };
        let Some(container) = target else {
            return false;
        };
        self.apply_balance(container)
    }

    fn apply_balance(&mut self, container: NodeId) -> bool {
        match self.nodes.get_mut(container) {
            Some(Node::Container { weights, .. }) => {
                weights.iter_mut().for_each(|w| *w = 1.0);
                true
            }
            _ => false,
        }
    }

    /// Walks up from `from` to the nearest ancestor container whose
    /// orientation matches `dir`'s axis and where `from` isn't already the
    /// boundary child, then descends into the neighboring branch's MRU
    /// leaf. Returns `None` if `from` is already at the tree's edge in
    /// `dir`. Read-only — see `focus_in_direction` for the version that
    /// also updates MRU.
    pub fn navigate(&self, from: NodeId, dir: Direction) -> Option<NodeId> {
        let axis = axis_for(dir);
        let forward = matches!(dir, Direction::Right | Direction::Down);

        let mut child = from;
        while let Some(&parent) = self.parents.get(&child) {
            if let Some(Node::Container {
                orientation,
                children,
                ..
            }) = self.nodes.get(parent)
                && *orientation == axis
            {
                let idx = children.iter().position(|&c| c == child)?;
                // Skips over `Floating` siblings — they have no spatial
                // footprint in `layout`, so directional navigation treats
                // them as if they weren't there rather than landing focus
                // on one.
                let mut cursor = step_cursor(Some(idx), forward);
                while let Some(&target) = cursor.and_then(|i| children.get(i)) {
                    if matches!(self.nodes.get(target), Some(Node::Floating { .. })) {
                        cursor = step_cursor(cursor, forward);
                        continue;
                    }
                    return Some(self.mru_leaf(target));
                }
            }
            child = parent;
        }
        None
    }

    /// The navigation entry point `WmState` actually calls: `navigate`
    /// plus recording the result as newly focused (so a subsequent
    /// `Accordion` peek or nested navigate lands where the user just went,
    /// not always at a container's first child). One algorithm for both
    /// `Tiles` and `Accordion` — an `Accordion`'s own orientation
    /// determines which directions cycle it, and hitting its edge
    /// escalates up to the next matching ancestor exactly like `Tiles`,
    /// rather than wrapping.
    pub fn focus_in_direction(&mut self, from: NodeId, dir: Direction) -> Option<NodeId> {
        let target = self.navigate(from, dir)?;
        self.record_focus(target);
        Some(target)
    }

    /// Moves `from` one step in `dir`, re-parenting it through the tree
    /// (not just swapping which window sits where):
    /// - Same-axis neighbor is a window: the two trade positions (and thus
    ///   footprints) within their shared parent.
    /// - Same-axis neighbor is a container: `from` moves into it, landing
    ///   at the edge nearest the boundary it just crossed.
    /// - No same-axis room at this level: escalate up (`move_out`) to the
    ///   nearest ancestor whose orientation matches, landing next to the
    ///   branch `from` climbed out of.
    /// - No such ancestor exists anywhere up to the root: wrap the whole
    ///   tree in a brand-new root container along `dir`'s axis (an implicit
    ///   container at the workspace boundary).
    ///
    /// Returns `false` if `from` is the tree's only window (nothing to
    /// move relative to).
    pub fn move_in_direction(&mut self, from: NodeId, dir: Direction) -> bool {
        let axis = axis_for(dir);
        let forward = matches!(dir, Direction::Right | Direction::Down);

        if let Some(&parent) = self.parents.get(&from)
            && self.orientation_matches(parent, axis)
            && self.move_within(from, parent, axis, forward)
        {
            self.record_focus(from);
            self.normalize();
            return true;
        }

        if self.move_out(from, axis, forward) {
            self.record_focus(from);
            self.normalize();
            return true;
        }

        self.wrap_root(from, axis, forward)
    }

    fn orientation_matches(&self, container: NodeId, axis: Orientation) -> bool {
        matches!(self.nodes.get(container), Some(Node::Container { orientation, .. }) if *orientation == axis)
    }

    fn move_within(
        &mut self,
        from: NodeId,
        parent: NodeId,
        axis: Orientation,
        forward: bool,
    ) -> bool {
        let Some(Node::Container { children, .. }) = self.nodes.get(parent) else {
            return false;
        };
        let Some(idx) = children.iter().position(|&c| c == from) else {
            return false;
        };

        // Skips over `Floating` siblings — they carry no footprint in
        // `layout`, so they shouldn't block a tiled window from moving past
        // them the way an actual tiled/container sibling does.
        let mut cursor = step_cursor(Some(idx), forward);
        let (target_idx, sibling) = loop {
            let Some(&candidate) = cursor.and_then(|i| children.get(i)) else {
                return false;
            };
            if matches!(self.nodes.get(candidate), Some(Node::Floating { .. })) {
                cursor = step_cursor(cursor, forward);
                continue;
            }
            break (cursor.expect("checked above"), candidate);
        };

        match self.nodes.get(sibling) {
            Some(Node::Window { .. }) => {
                if let Some(Node::Container {
                    children, weights, ..
                }) = self.nodes.get_mut(parent)
                {
                    children.swap(idx, target_idx);
                    weights.swap(idx, target_idx);
                }
                true
            }
            Some(Node::Container { .. }) => {
                let anchor = self.entry_point(sibling, axis, forward);
                if anchor == from {
                    return false;
                }
                self.detach(from, parent);
                self.insert_sibling(from, anchor, forward);
                true
            }
            _ => false,
        }
    }

    /// Climbs from `from`'s immediate parent looking for an ancestor
    /// container whose orientation matches `axis`, then re-parents `from`
    /// as a new sibling of the branch it climbed out of, on the side
    /// facing `dir`. Returns `false` if no such ancestor exists (the
    /// caller falls back to `wrap_root`).
    fn move_out(&mut self, from: NodeId, axis: Orientation, forward: bool) -> bool {
        let Some(&from_parent) = self.parents.get(&from) else {
            return false;
        };
        let mut branch = from_parent;
        while let Some(&ancestor) = self.parents.get(&branch) {
            if self.orientation_matches(ancestor, axis) {
                self.detach(from, from_parent);
                self.insert_sibling(from, branch, forward);
                return true;
            }
            branch = ancestor;
        }
        false
    }

    /// Wraps the entire tree in a fresh root container along `axis`,
    /// placing `from` on the side of the old root that `forward` faces. If
    /// detaching `from` collapses the old root, the replacement preserves
    /// its layout; otherwise the new outer container uses `Tiles`.
    /// Returns `false` if `from` is already the whole tree (the lone root
    /// window — nothing to wrap it against).
    fn wrap_root(&mut self, from: NodeId, axis: Orientation, forward: bool) -> bool {
        let Some(old_root) = self.root else {
            return false;
        };
        if old_root == from {
            return false;
        }
        let Some(&from_parent) = self.parents.get(&from) else {
            return false;
        };
        let layout = match self.nodes.get(old_root) {
            Some(Node::Container {
                layout, children, ..
            }) if from_parent == old_root && children.len() == 2 => *layout,
            _ => Layout::Tiles,
        };
        self.detach(from, from_parent);

        let children = if forward {
            vec![old_root, from]
        } else {
            vec![from, old_root]
        };
        let new_root = self.nodes.insert(Node::Container {
            layout,
            orientation: axis,
            children,
            weights: vec![1.0, 1.0],
            mru: usize::from(forward),
        });
        self.parents.insert(old_root, new_root);
        self.parents.insert(from, new_root);
        self.root = Some(new_root);

        self.record_focus(from);
        self.normalize();
        true
    }

    /// Finds the leaf nearest the edge of `container` that faces the
    /// direction traveled from (i.e. the edge a window entering `container`
    /// from that side would land at): descends through containers whose
    /// orientation matches `axis` via their near edge (index `0` if
    /// `forward`, else the last index), and through orientation-mismatched
    /// containers via MRU (there's no inherent near/far edge on an axis a
    /// container doesn't split on, so MRU is the closest thing to "the
    /// child the user was last looking at here").
    fn entry_point(&self, container: NodeId, axis: Orientation, forward: bool) -> NodeId {
        match self.nodes.get(container) {
            Some(Node::Container {
                orientation,
                children,
                mru,
                ..
            }) if !children.is_empty() => {
                let next = if *orientation == axis {
                    children[if forward { 0 } else { children.len() - 1 }]
                } else {
                    children[(*mru).min(children.len() - 1)]
                };
                self.entry_point(next, axis, forward)
            }
            _ => container,
        }
    }

    /// Computes each window's frame within `area` by recursively dividing
    /// it per the tree's containers, applying `gaps` — outer padding around
    /// the whole area, inner spacing between `Tiles` siblings, and
    /// `accordion` peek padding for non-focused `Accordion` siblings (see
    /// the `Gaps` struct doc comment).
    pub fn layout(&self, area: Rect, gaps: Gaps) -> Vec<(WindowId, Rect)> {
        let mut out = Vec::new();
        if let Some(root) = self.root {
            let padded = apply_outer_gaps(area, self.effective_outer(&gaps));
            self.layout_node(root, padded, gaps.inner, gaps.accordion, &mut out);
        }
        out
    }

    /// `gaps.outer_solo` in place of `gaps.outer` when the workspace has
    /// exactly one tile (floating windows don't count), else `gaps.outer`
    /// unchanged. Shared by `layout` and `resize_handle_at` so hit-testing
    /// geometry never diverges from what was actually drawn.
    fn effective_outer(&self, gaps: &Gaps) -> (f64, f64, f64, f64) {
        match gaps.outer_solo {
            Some(outer_solo) if self.tiled_window_ids().len() == 1 => outer_solo,
            _ => gaps.outer,
        }
    }

    /// Shared by `layout_node` and `resize_handle_node`'s `Tiles` branches:
    /// the sizeable (non-`Floating`) children with their weights, the total
    /// weight (floored at `f32::EPSILON` to avoid a division by zero), and
    /// the space actually divisible among them once `inner_gap` is
    /// subtracted for the gaps between them. `None` if every child is
    /// `Floating` (nothing to size).
    fn tiles_layout_inputs(
        &self,
        children: &[NodeId],
        weights: &[f32],
        orientation: Orientation,
        area: Rect,
        inner_gap: f64,
    ) -> Option<TilesLayoutInputs> {
        let sizeable: Vec<(NodeId, f32)> = children
            .iter()
            .zip(weights.iter())
            .filter(|&(&c, _)| !matches!(self.nodes.get(c), Some(Node::Floating { .. })))
            .map(|(&c, &w)| (c, w))
            .collect();
        let n = sizeable.len();
        if n == 0 {
            return None;
        }
        let total: f32 = sizeable
            .iter()
            .map(|&(_, w)| w)
            .sum::<f32>()
            .max(f32::EPSILON);
        let total_gap = inner_gap * (n.saturating_sub(1)) as f64;
        let divisible = match orientation {
            Orientation::Horizontal => (area.width - total_gap).max(0.0),
            Orientation::Vertical => (area.height - total_gap).max(0.0),
        };
        Some((sizeable, total, divisible))
    }

    fn layout_node(
        &self,
        node: NodeId,
        area: Rect,
        inner_gap: f64,
        accordion_padding: f64,
        out: &mut Vec<(WindowId, Rect)>,
    ) {
        match self.nodes.get(node) {
            Some(Node::Window { window }) => out.push((*window, area)),
            // A floating leaf owns no space in its parent's split — its
            // real frame is computed/written by `tili-daemon` separately
            // (see `Node::Floating`'s doc comment), so it's simply skipped
            // here rather than emitted with some placeholder rect.
            Some(Node::Floating { .. }) => {}
            Some(Node::Container {
                layout: Layout::Tiles,
                orientation,
                children,
                weights,
                ..
            }) => {
                // Floating children are excluded from the split entirely —
                // they take no share of `area` and don't count toward the
                // inner-gap total, so tiled siblings divide the space
                // exactly as if the floating child weren't there.
                let Some((sizeable, total, divisible)) =
                    self.tiles_layout_inputs(children, weights, *orientation, area, inner_gap)
                else {
                    return;
                };
                let mut offset = 0.0_f64;
                for (child, weight) in sizeable {
                    let fraction = f64::from(weight / total);
                    let child_size = divisible * fraction;
                    let child_area = split_child_area(area, *orientation, offset, child_size);
                    offset += child_size + inner_gap;
                    self.layout_node(child, child_area, inner_gap, accordion_padding, out);
                }
            }
            Some(Node::Container {
                layout: Layout::Accordion,
                orientation,
                children,
                ..
            }) => {
                // Same exclusion as the `Tiles` branch above — a floating
                // child doesn't get an accordion slot (or count toward
                // its neighbors' peek padding).
                let sizeable: Vec<NodeId> = children
                    .iter()
                    .copied()
                    .filter(|&c| !matches!(self.nodes.get(c), Some(Node::Floating { .. })))
                    .collect();
                let n = sizeable.len();
                for (i, &child) in sizeable.iter().enumerate() {
                    let child_area =
                        accordion_child_area(area, *orientation, i, n, accordion_padding);
                    self.layout_node(child, child_area, inner_gap, accordion_padding, out);
                }
            }
            None => {}
        }
    }

    /// Finds the resize border (if any) at `point`, given the same `area`/`gaps` `layout` would
    /// be called with — a hit here always agrees with what was actually drawn, since it mirrors
    /// `layout_node`'s own `Tiles`/`Accordion` geometry. Returns `None` anywhere the tree has no
    /// internal border to hit at all, including a lone root window or an all-`Accordion` tree —
    /// the same "nothing to resize" guarantee `resize_weight` already enforces, but derived
    /// structurally here rather than checked separately.
    pub fn resize_handle_at(
        &self,
        area: Rect,
        gaps: Gaps,
        point: (f64, f64),
    ) -> Option<ResizeHandle> {
        let root = self.root?;
        let padded = apply_outer_gaps(area, self.effective_outer(&gaps));
        self.resize_handle_node(root, padded, gaps.inner, gaps.accordion, point)
    }

    fn resize_handle_node(
        &self,
        node: NodeId,
        area: Rect,
        inner_gap: f64,
        accordion_padding: f64,
        point: (f64, f64),
    ) -> Option<ResizeHandle> {
        match self.nodes.get(node)? {
            Node::Window { .. } | Node::Floating { .. } => None,
            Node::Container {
                layout: Layout::Tiles,
                orientation,
                children,
                weights,
                ..
            } => {
                let (sizeable, total, divisible) =
                    self.tiles_layout_inputs(children, weights, *orientation, area, inner_gap)?;
                let n = sizeable.len();
                let weight_per_pixel = if divisible > 0.0 {
                    f64::from(total) / divisible
                } else {
                    0.0
                };

                let mut offset = 0.0_f64;
                for (i, &(child, weight)) in sizeable.iter().enumerate() {
                    let fraction = f64::from(weight / total);
                    let child_size = divisible * fraction;
                    let child_area = split_child_area(area, *orientation, offset, child_size);

                    let past_child = match orientation {
                        Orientation::Horizontal => point.0 >= child_area.x + child_area.width,
                        Orientation::Vertical => point.1 >= child_area.y + child_area.height,
                    };
                    if !past_child {
                        return self.resize_handle_node(
                            child,
                            child_area,
                            inner_gap,
                            accordion_padding,
                            point,
                        );
                    }

                    // Both ends inclusive — with a zero inner gap the border is a single point
                    // (child edges touch exactly), and the caller queries exactly a window's own
                    // last-known edge coordinate, so the band must include both of its endpoints
                    // to reliably hit even a zero-width gap.
                    let in_gap = i + 1 < n
                        && match orientation {
                            Orientation::Horizontal => {
                                point.0 <= child_area.x + child_area.width + inner_gap
                                    && point.1 >= area.y
                                    && point.1 < area.y + area.height
                            }
                            Orientation::Vertical => {
                                point.1 <= child_area.y + child_area.height + inner_gap
                                    && point.0 >= area.x
                                    && point.0 < area.x + area.width
                            }
                        };
                    if in_gap {
                        let (next_child, _) = sizeable[i + 1];
                        return Some(ResizeHandle {
                            before: child,
                            after: next_child,
                            orientation: *orientation,
                            weight_per_pixel,
                        });
                    }

                    offset += child_size + inner_gap;
                }
                None
            }
            Node::Container {
                layout: Layout::Accordion,
                orientation,
                children,
                mru,
                ..
            } => {
                let sizeable: Vec<NodeId> = children
                    .iter()
                    .copied()
                    .filter(|&c| !matches!(self.nodes.get(c), Some(Node::Floating { .. })))
                    .collect();
                let n = sizeable.len();
                let idx = (*mru).min(n.checked_sub(1)?);
                let child_area =
                    accordion_child_area(area, *orientation, idx, n, accordion_padding);
                self.resize_handle_node(
                    sizeable[idx],
                    child_area,
                    inner_gap,
                    accordion_padding,
                    point,
                )
            }
        }
    }

    /// Runs both normalizations to restore the tree's two invariants (see
    /// the struct doc comment) after a mutation. Always safe to call more
    /// than needed — small tree sizes (a WM's window count) make the
    /// simplicity of "just rescan" worth more than avoiding a redundant pass.
    fn normalize(&mut self) {
        self.flatten();
        self.merge_same_orientation();
        self.flatten();
    }

    /// Promotes any single-child container's child up into its own slot,
    /// repeating until none remain — including all the way up to `self.root`
    /// itself, which is how a root container ever collapses back down to a
    /// bare `Window` leaf with no container at all.
    fn flatten(&mut self) {
        loop {
            let target = self.nodes.iter().find_map(|(id, node)| match node {
                Node::Container { children, .. } if children.len() == 1 => Some((id, children[0])),
                _ => None,
            });
            let Some((container_id, only_child)) = target else {
                break;
            };
            let parent = self.parents.remove(&container_id);
            self.nodes.remove(container_id);
            match parent {
                Some(p) => {
                    self.replace_child(p, container_id, only_child);
                    self.parents.insert(only_child, p);
                }
                None => {
                    self.root = Some(only_child);
                    self.parents.remove(&only_child);
                }
            }
        }
    }

    /// Splices any `Tiles` container's children directly into its `Tiles`
    /// parent when they share an orientation, repeating until none remain.
    /// `Accordion` containers are never merge candidates (in either
    /// direction) — an accordion nested in a same-orientation tiles parent
    /// (or vice versa) is a meaningful, distinct structure, not redundant
    /// nesting.
    fn merge_same_orientation(&mut self) {
        loop {
            let target = self.nodes.iter().find_map(|(id, node)| {
                let Node::Container {
                    layout: Layout::Tiles,
                    orientation,
                    ..
                } = node
                else {
                    return None;
                };
                let &parent_id = self.parents.get(&id)?;
                let Some(Node::Container {
                    layout: Layout::Tiles,
                    orientation: parent_orientation,
                    ..
                }) = self.nodes.get(parent_id)
                else {
                    return None;
                };
                (orientation == parent_orientation).then_some((id, parent_id))
            });
            let Some((child_id, parent_id)) = target else {
                break;
            };
            self.splice_container(child_id, parent_id);
        }
    }

    fn splice_container(&mut self, child_id: NodeId, parent_id: NodeId) {
        let Some(Node::Container {
            children: grandkids,
            weights: gk_weights,
            mru: gk_mru,
            ..
        }) = self.nodes.remove(child_id)
        else {
            return;
        };
        self.parents.remove(&child_id);

        let Some(Node::Container {
            children, weights, ..
        }) = self.nodes.get(parent_id)
        else {
            return;
        };
        let Some(idx) = children.iter().position(|&c| c == child_id) else {
            return;
        };
        let own_weight = weights[idx];
        let gk_total: f32 = gk_weights.iter().sum::<f32>().max(f32::EPSILON);
        let scaled: Vec<f32> = gk_weights
            .iter()
            .map(|w| own_weight * w / gk_total)
            .collect();
        let was_mru_branch =
            matches!(self.nodes.get(parent_id), Some(Node::Container { mru, .. }) if *mru == idx);

        let Some(Node::Container {
            children,
            weights,
            mru,
            ..
        }) = self.nodes.get_mut(parent_id)
        else {
            return;
        };
        children.remove(idx);
        weights.remove(idx);
        for (i, (&gc, &w)) in grandkids.iter().zip(scaled.iter()).enumerate() {
            children.insert(idx + i, gc);
            weights.insert(idx + i, w);
        }

        if was_mru_branch {
            *mru = idx + gk_mru;
        } else if *mru > idx {
            *mru += grandkids.len() - 1;
        }
        if *mru >= children.len() {
            *mru = children.len().saturating_sub(1);
        }

        for &gc in &grandkids {
            self.parents.insert(gc, parent_id);
        }
    }

    fn replace_child(&mut self, parent: NodeId, old: NodeId, new: NodeId) {
        if let Some(Node::Container { children, .. }) = self.nodes.get_mut(parent)
            && let Some(pos) = children.iter().position(|&c| c == old)
        {
            children[pos] = new;
        }
    }

    /// Descends through containers via their `mru` child (falling back to
    /// the first child if `mru` is somehow out of range) until reaching a
    /// `Window` leaf.
    fn mru_leaf(&self, mut node: NodeId) -> NodeId {
        loop {
            match self.nodes.get(node) {
                Some(Node::Container { children, mru, .. }) => {
                    match children.get(*mru).or_else(|| children.first()) {
                        Some(&next) => node = next,
                        None => return node,
                    }
                }
                _ => return node,
            }
        }
    }
}

/// Every index into `children` other than `idx`, plus their combined
/// weight — shared by `apply_resize` and `branch_resize_bounds`, which must
/// agree on exactly the same set of "other" siblings and total.
fn other_indices_and_total(children: &[NodeId], weights: &[f32], idx: usize) -> (Vec<usize>, f32) {
    let others: Vec<usize> = (0..children.len()).filter(|&i| i != idx).collect();
    let others_total: f32 = others.iter().map(|&i| weights[i]).sum();
    (others, others_total)
}

/// Steps `cursor` one child index forward or backward, saturating to `None`
/// past either end — shared by `navigate`/`move_within`'s "skip over a
/// `Floating` sibling" loops, both for their first step and each retry.
fn step_cursor(cursor: Option<usize>, forward: bool) -> Option<usize> {
    if forward {
        cursor.and_then(|i| i.checked_add(1))
    } else {
        cursor.and_then(|i| i.checked_sub(1))
    }
}

fn axis_for(dir: Direction) -> Orientation {
    match dir {
        Direction::Left | Direction::Right => Orientation::Horizontal,
        Direction::Up | Direction::Down => Orientation::Vertical,
    }
}

/// Insets `area` by `outer` (top, right, bottom, left) — shared by `layout`
/// and `resize_handle_at`, which must agree on exactly the same padded area
/// for a resize-handle hit to agree with what `layout` actually drew.
fn apply_outer_gaps(area: Rect, outer: (f64, f64, f64, f64)) -> Rect {
    let (top, right, bottom, left) = outer;
    Rect {
        x: area.x + left,
        y: area.y + top,
        width: (area.width - left - right).max(0.0),
        height: (area.height - top - bottom).max(0.0),
    }
}

/// One `Tiles` child's rect: `child_size` along the split axis starting at
/// `offset` from `area`'s edge, the full cross-axis extent otherwise —
/// shared by `layout_node` and `resize_handle_node`, which must agree on
/// exactly the same child geometry.
fn split_child_area(area: Rect, orientation: Orientation, offset: f64, child_size: f64) -> Rect {
    match orientation {
        Orientation::Horizontal => Rect {
            x: area.x + offset,
            y: area.y,
            width: child_size,
            height: area.height,
        },
        Orientation::Vertical => Rect {
            x: area.x,
            y: area.y + offset,
            width: area.width,
            height: child_size,
        },
    }
}

/// One `Accordion` child's rect at index `idx` of `n`: `area` inset by
/// `accordion_padding` on whichever edges have a sibling peeking out (every
/// edge but the first/last) — shared by `layout_node` and
/// `resize_handle_node`, which must agree on exactly the same child
/// geometry.
fn accordion_child_area(
    area: Rect,
    orientation: Orientation,
    idx: usize,
    n: usize,
    accordion_padding: f64,
) -> Rect {
    let mut child_area = area;
    let pad_before = idx > 0;
    let pad_after = idx + 1 < n;
    match orientation {
        Orientation::Horizontal => {
            if pad_before {
                child_area.x += accordion_padding;
                child_area.width -= accordion_padding;
            }
            if pad_after {
                child_area.width -= accordion_padding;
            }
        }
        Orientation::Vertical => {
            if pad_before {
                child_area.y += accordion_padding;
                child_area.height -= accordion_padding;
            }
            if pad_after {
                child_area.height -= accordion_padding;
            }
        }
    }
    child_area.width = child_area.width.max(0.0);
    child_area.height = child_area.height.max(0.0);
    child_area
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

    fn insert(tree: &mut Tree, window: WindowId, near: Option<NodeId>) -> NodeId {
        tree.insert_window(window, near, Orientation::Horizontal)
    }

    /// Widths of every window in `layout`, as a lookup by window id — the
    /// tests below check proportions, not raw pixels, so `f64` rounding
    /// noise from weight division doesn't cause flakiness.
    fn width_of(layout: &[(WindowId, Rect)], window: WindowId) -> f64 {
        layout.iter().find(|(w, _)| *w == window).unwrap().1.width
    }

    fn height_of(layout: &[(WindowId, Rect)], window: WindowId) -> f64 {
        layout.iter().find(|(w, _)| *w == window).unwrap().1.height
    }

    #[test]
    fn empty_tree_has_no_root() {
        let tree = Tree::new();
        assert_eq!(tree.root(), None);
        assert!(tree.is_empty());
        assert!(tree.layout(area(), Gaps::default()).is_empty());
    }

    #[test]
    fn first_window_becomes_root_with_no_container() {
        let mut tree = Tree::new();
        let leaf = insert(&mut tree, 1, None);
        assert_eq!(tree.root(), Some(leaf));
        assert_eq!(tree.window_at(leaf), Some(1));
        assert_eq!(
            tree.orientation_of(leaf),
            None,
            "lone window has no parent container"
        );

        let layout = tree.layout(area(), Gaps::default());
        assert_eq!(layout, vec![(1, area())]);
    }

    #[test]
    fn second_window_splits_side_by_side_and_fills_area() {
        let mut tree = Tree::new();
        let first = insert(&mut tree, 1, None);
        insert(&mut tree, 2, Some(first));

        let layout = tree.layout(area(), Gaps::default());
        assert_eq!(layout.len(), 2);

        let total_width: f64 = layout.iter().map(|(_, r)| r.width).sum();
        assert!((total_width - area().width).abs() < f64::EPSILON);
        for (_, rect) in &layout {
            assert_eq!(rect.height, area().height);
            assert_eq!(rect.y, 0.0);
        }
    }

    #[test]
    fn second_window_uses_tiles_by_default() {
        let mut tree = Tree::new();
        let first = insert(&mut tree, 1, None);
        insert(&mut tree, 2, Some(first));
        assert!(!tree.is_root_accordion());
    }

    #[test]
    fn set_default_layout_applies_to_first_container_only() {
        let mut tree = Tree::new();
        tree.set_default_layout(Layout::Accordion);
        let first = insert(&mut tree, 1, None);
        insert(&mut tree, 2, Some(first));
        assert!(tree.is_root_accordion());
    }

    #[test]
    fn three_windows_inserted_sequentially_are_three_equal_flat_siblings() {
        // Regression test for the old binary-tree bug: inserting next to
        // the most-recently-inserted window used to nest into a fresh
        // 2-child split each time (50/25/25), instead of joining the same
        // flat container as an equal third sibling.
        let mut tree = Tree::new();
        let w1 = insert(&mut tree, 1, None);
        let w2 = insert(&mut tree, 2, Some(w1));
        insert(&mut tree, 3, Some(w2));

        let layout = tree.layout(area(), Gaps::default());
        assert_eq!(layout.len(), 3);
        let w1_width = width_of(&layout, 1);
        let w2_width = width_of(&layout, 2);
        let w3_width = width_of(&layout, 3);
        assert!((w1_width - w2_width).abs() < 0.01);
        assert!((w2_width - w3_width).abs() < 0.01);
    }

    #[test]
    fn root_orientation_hint_only_applies_when_creating_the_first_container() {
        let mut tree = Tree::new();
        let w1 = tree.insert_window(1, None, Orientation::Vertical);
        // Orientation is irrelevant for the very first (containerless) insert.
        assert_eq!(tree.orientation_of(w1), None);

        tree.insert_window(2, Some(w1), Orientation::Vertical);
        assert_eq!(tree.root_orientation(), Some(Orientation::Vertical));

        let layout = tree.layout(area(), Gaps::default());
        for (_, rect) in &layout {
            assert_eq!(
                rect.width,
                area().width,
                "vertical root stacks full-width rows"
            );
        }
    }

    #[test]
    fn gaps_shrink_outer_area_and_separate_siblings() {
        let mut tree = Tree::new();
        let first = insert(&mut tree, 1, None);
        insert(&mut tree, 2, Some(first));

        let gaps = Gaps {
            inner: 10.0,
            outer: (20.0, 20.0, 20.0, 20.0),
            outer_solo: None,
            accordion: 0.0,
        };
        let layout = tree.layout(area(), gaps);
        assert_eq!(layout.len(), 2);

        let leftmost = layout
            .iter()
            .min_by(|a, b| a.1.x.total_cmp(&b.1.x))
            .unwrap();
        assert_eq!(leftmost.1.x, 20.0);
        assert_eq!(leftmost.1.y, 20.0);
        assert_eq!(leftmost.1.height, area().height - 40.0);

        let total_width: f64 = layout.iter().map(|(_, r)| r.width).sum();
        let padded_width = area().width - 40.0;
        assert!((total_width + gaps.inner - padded_width).abs() < f64::EPSILON);
    }

    #[test]
    fn outer_solo_applies_when_exactly_one_tile() {
        let mut tree = Tree::new();
        insert(&mut tree, 1, None);

        let gaps = Gaps {
            outer: (20.0, 20.0, 20.0, 20.0),
            outer_solo: Some((40.0, 40.0, 40.0, 40.0)),
            ..Gaps::default()
        };
        let layout = tree.layout(area(), gaps);
        assert_eq!(
            layout,
            vec![(
                1,
                Rect {
                    x: 40.0,
                    y: 40.0,
                    width: area().width - 80.0,
                    height: area().height - 80.0,
                }
            )]
        );
    }

    #[test]
    fn outer_solo_unset_falls_back_to_outer_for_a_single_tile() {
        let mut tree = Tree::new();
        insert(&mut tree, 1, None);

        let gaps = Gaps {
            outer: (20.0, 20.0, 20.0, 20.0),
            outer_solo: None,
            ..Gaps::default()
        };
        let layout = tree.layout(area(), gaps);
        assert_eq!(
            layout,
            vec![(
                1,
                Rect {
                    x: 20.0,
                    y: 20.0,
                    width: area().width - 40.0,
                    height: area().height - 40.0,
                }
            )]
        );
    }

    #[test]
    fn outer_solo_is_ignored_once_a_second_tile_exists() {
        let mut tree = Tree::new();
        let first = insert(&mut tree, 1, None);
        insert(&mut tree, 2, Some(first));

        let gaps = Gaps {
            outer: (20.0, 20.0, 20.0, 20.0),
            outer_solo: Some((40.0, 40.0, 40.0, 40.0)),
            ..Gaps::default()
        };
        let layout = tree.layout(area(), gaps);
        let leftmost = layout
            .iter()
            .min_by(|a, b| a.1.x.total_cmp(&b.1.x))
            .unwrap();
        assert_eq!(leftmost.1.x, 20.0);
        assert_eq!(leftmost.1.y, 20.0);
    }

    #[test]
    fn outer_solo_still_applies_with_a_floating_sibling_present() {
        let mut tree = Tree::new();
        let w1 = insert(&mut tree, 1, None);
        tree.insert_floating(2, Some(w1), Orientation::Horizontal);

        let gaps = Gaps {
            outer: (20.0, 20.0, 20.0, 20.0),
            outer_solo: Some((40.0, 40.0, 40.0, 40.0)),
            ..Gaps::default()
        };
        // The floating window still gets no rect, and the lone tile still
        // counts as "solo" — floating windows don't count toward the tile
        // count that picks `outer` vs `outer_solo`.
        let layout = tree.layout(area(), gaps);
        assert_eq!(
            layout,
            vec![(
                1,
                Rect {
                    x: 40.0,
                    y: 40.0,
                    width: area().width - 80.0,
                    height: area().height - 80.0,
                }
            )]
        );
    }

    #[test]
    fn toggle_layout_converts_tiles_to_accordion_and_back_preserving_mru() {
        let mut tree = Tree::new();
        let a = insert(&mut tree, 1, None);
        let b = insert(&mut tree, 2, Some(a));
        tree.record_focus(b);
        assert!(!tree.is_accordion_container(a));

        assert!(tree.toggle_layout(a));
        assert!(tree.is_accordion_container(a));
        assert!(tree.is_accordion_container(b));

        // MRU (window 2, focused above) is still what shows fully — with
        // zero padding, every accordion child gets the exact same frame, so
        // this really just checks both windows still exist and layout
        // didn't blow up on the layout-kind flip.
        let layout = tree.layout(area(), Gaps::default());
        assert_eq!(layout.len(), 2);

        assert!(tree.toggle_layout(a));
        assert!(!tree.is_accordion_container(a));
        let layout = tree.layout(area(), Gaps::default());
        assert_eq!(layout.len(), 2, "back to Tiles, both windows tile again");
    }

    #[test]
    fn toggle_layout_on_lone_root_window_is_a_no_op() {
        let mut tree = Tree::new();
        let only = insert(&mut tree, 1, None);
        assert!(!tree.toggle_layout(only));
    }

    #[test]
    fn toggle_root_layout_targets_root_not_a_deeply_nested_parent() {
        let mut tree = Tree::new();
        let w1 = insert(&mut tree, 1, None);
        insert(&mut tree, 2, Some(w1));
        insert(&mut tree, 3, Some(w1));
        // Flat n-ary insert means all three share the root container
        // directly now — toggling root affects all three at once.

        assert!(tree.toggle_root_layout());
        assert!(tree.is_root_accordion());
        assert!(tree.toggle_root_layout());
        assert!(!tree.is_root_accordion());
    }

    #[test]
    fn toggle_root_layout_on_lone_root_window_is_a_no_op() {
        let mut tree = Tree::new();
        insert(&mut tree, 1, None);
        assert!(!tree.toggle_root_layout());
    }

    #[test]
    fn set_orientation_then_navigate_up_down_works() {
        let mut tree = Tree::new();
        let w1 = insert(&mut tree, 1, None);
        let w2 = insert(&mut tree, 2, Some(w1));
        // Default root orientation from `insert` helper is Horizontal, so
        // up/down should find nothing yet.
        assert_eq!(tree.navigate(w1, Direction::Down), None);

        assert!(tree.set_orientation(w1, Orientation::Vertical));
        assert_eq!(tree.navigate(w1, Direction::Down), Some(w2));
        assert_eq!(tree.navigate(w1, Direction::Right), None);
    }

    #[test]
    fn merge_splices_same_orientation_child_into_parent() {
        let mut tree = Tree::new();
        let w1 = insert(&mut tree, 1, None);
        let w2 = insert(&mut tree, 2, Some(w1));
        insert(&mut tree, 3, Some(w2));
        // root(h) = [1, 2, 3] flat.
        assert!(tree.join_with(w2, Direction::Right)); // wraps 2,3 into a nested vertical container
        assert_eq!(tree.orientation_of(w2), Some(Orientation::Vertical));
        assert_eq!(tree.window_ids().len(), 3);

        // Flip the nested container back to horizontal, matching the root
        // — normalize's merge pass should splice its children straight
        // into the root rather than leaving a redundant nested
        // same-orientation container.
        assert!(tree.set_orientation(w2, Orientation::Horizontal));
        assert_eq!(tree.orientation_of(w2), tree.orientation_of(w1));

        let layout = tree.layout(area(), Gaps::default());
        assert_eq!(layout.len(), 3);
        let w1_width = width_of(&layout, 1);
        let w2_width = width_of(&layout, 2);
        let w3_width = width_of(&layout, 3);
        assert!((w1_width - w2_width).abs() < 0.01);
        assert!((w2_width - w3_width).abs() < 0.01);
    }

    #[test]
    fn join_with_wraps_two_leaves_in_a_perpendicular_container() {
        let mut tree = Tree::new();
        let w1 = insert(&mut tree, 1, None);
        let w2 = insert(&mut tree, 2, Some(w1));
        assert_eq!(tree.root_orientation(), Some(Orientation::Horizontal));

        assert!(tree.join_with(w1, Direction::Right));
        // w1 and w2 are now stacked vertically inside their own container.
        assert_eq!(tree.orientation_of(w1), Some(Orientation::Vertical));
        assert_eq!(tree.orientation_of(w2), Some(Orientation::Vertical));

        let layout = tree.layout(area(), Gaps::default());
        for (_, rect) in &layout {
            assert_eq!(rect.width, area().width, "joined pair stacks full-width");
        }
    }

    #[test]
    fn join_with_no_neighbor_in_that_direction_is_a_no_op() {
        let mut tree = Tree::new();
        let w1 = insert(&mut tree, 1, None);
        insert(&mut tree, 2, Some(w1));
        assert!(
            !tree.join_with(w1, Direction::Left),
            "w1 is already leftmost"
        );
        assert!(
            !tree.join_with(w1, Direction::Down),
            "root is horizontal, not vertical"
        );
    }

    #[test]
    fn navigate_left_right_across_a_horizontal_root() {
        let mut tree = Tree::new();
        let left = insert(&mut tree, 1, None);
        let right_leaf = insert(&mut tree, 2, Some(left));

        assert_eq!(tree.navigate(left, Direction::Right), Some(right_leaf));
        assert_eq!(tree.navigate(right_leaf, Direction::Left), Some(left));
        assert_eq!(tree.navigate(left, Direction::Up), None);
        assert_eq!(tree.navigate(left, Direction::Down), None);
        assert_eq!(tree.navigate(left, Direction::Left), None);
    }

    #[test]
    fn focus_in_direction_records_mru_for_later_descent() {
        let mut tree = Tree::new();
        let w1 = insert(&mut tree, 1, None);
        let w2 = insert(&mut tree, 2, Some(w1));
        let w3 = insert(&mut tree, 3, Some(w2));
        assert!(tree.join_with(w2, Direction::Right)); // wraps 2,3 into a nested vertical container

        // Right after the join, the nested container's MRU is w2 (it was
        // the node the join pivoted on) — navigating in from w1 should
        // land there.
        assert_eq!(tree.focus_in_direction(w1, Direction::Right), Some(w2));

        // Focus w3 directly, then navigate away and back — the nested
        // container's MRU should now be w3, not fall back to w2.
        tree.record_focus(w3);
        assert_eq!(
            tree.focus_in_direction(w1, Direction::Left),
            None,
            "already leftmost"
        );
        assert_eq!(tree.focus_in_direction(w1, Direction::Right), Some(w3));
    }

    #[test]
    fn move_within_swaps_two_windows() {
        // A `move` re-parents by array position, not by rewriting which
        // window a `NodeId` points to (that binding is permanent — see the
        // struct doc comment) — so after the swap each `NodeId` still
        // carries its original window, but the two windows' on-screen
        // rects trade places.
        let mut tree = Tree::new();
        let a = insert(&mut tree, 1, None);
        let b = insert(&mut tree, 2, Some(a));

        let before = tree.layout(area(), Gaps::default());
        let a_rect_before = *before.iter().find(|(w, _)| *w == 1).unwrap();
        let b_rect_before = *before.iter().find(|(w, _)| *w == 2).unwrap();

        assert!(tree.move_in_direction(a, Direction::Right));
        assert_eq!(tree.window_at(a), Some(1));
        assert_eq!(tree.window_at(b), Some(2));

        let after = tree.layout(area(), Gaps::default());
        let a_rect_after = *after.iter().find(|(w, _)| *w == 1).unwrap();
        let b_rect_after = *after.iter().find(|(w, _)| *w == 2).unwrap();
        assert_eq!(a_rect_after.1, b_rect_before.1);
        assert_eq!(b_rect_after.1, a_rect_before.1);
    }

    #[test]
    fn move_within_swaps_weights_along_with_windows() {
        // Give window 1 a bigger share of the split before moving it, so
        // a swap that only reorders `children` (forgetting `weights`) would
        // leave the *sizes* bound to array position instead of following
        // each window across the move.
        let mut tree = Tree::new();
        let a = insert(&mut tree, 1, None);
        insert(&mut tree, 2, Some(a));
        assert!(tree.resize_weight(a, 0.2));

        let before = tree.layout(area(), Gaps::default());
        let a_width_before = before.iter().find(|(w, _)| *w == 1).unwrap().1.width;
        let b_width_before = before.iter().find(|(w, _)| *w == 2).unwrap().1.width;
        assert_ne!(a_width_before, b_width_before);

        assert!(tree.move_in_direction(a, Direction::Right));

        let after = tree.layout(area(), Gaps::default());
        let a_width_after = after.iter().find(|(w, _)| *w == 1).unwrap().1.width;
        let b_width_after = after.iter().find(|(w, _)| *w == 2).unwrap().1.width;
        assert_eq!(a_width_after, a_width_before);
        assert_eq!(b_width_after, b_width_before);
    }

    #[test]
    fn move_out_reparents_into_perpendicular_ancestor() {
        // root(h) = [1, joined-container(v) = [2, 3]]
        let mut tree = Tree::new();
        let w1 = insert(&mut tree, 1, None);
        let w2 = insert(&mut tree, 2, Some(w1));
        insert(&mut tree, 3, Some(w2));
        assert!(tree.join_with(w2, Direction::Right)); // wraps w2,w3 vertically
        assert_eq!(tree.orientation_of(w2), Some(Orientation::Vertical));

        // Moving w2 Left (root's axis) should climb out of the vertical
        // pair and land back in the horizontal root, next to window 1.
        assert!(tree.move_in_direction(w2, Direction::Left));
        assert_eq!(tree.orientation_of(w2), Some(Orientation::Horizontal));
        assert_eq!(tree.window_ids().len(), 3);
    }

    #[test]
    fn move_wraps_root_when_no_matching_ancestor_exists() {
        let mut tree = Tree::new();
        let w1 = insert(&mut tree, 1, None);
        let w2 = insert(&mut tree, 2, Some(w1));
        // Root is horizontal; moving Down has no vertical ancestor anywhere.
        assert!(tree.move_in_direction(w2, Direction::Down));
        assert_eq!(tree.root_orientation(), Some(Orientation::Vertical));
        assert_eq!(tree.window_ids().len(), 2);
    }

    #[test]
    fn move_perpendicular_to_two_window_accordion_preserves_layout() {
        let mut tree = Tree::new();
        let top = insert(&mut tree, 1, None);
        let bottom = insert(&mut tree, 2, Some(top));
        assert!(tree.set_root_orientation(Orientation::Vertical));
        assert!(tree.toggle_root_layout());

        assert!(tree.move_in_direction(bottom, Direction::Left));
        assert!(tree.is_root_accordion());
        assert_eq!(tree.root_orientation(), Some(Orientation::Horizontal));

        let layout = tree.layout(
            area(),
            Gaps {
                accordion: 20.0,
                ..Gaps::default()
            },
        );
        assert!(
            layout.iter().find(|(w, _)| *w == 2).unwrap().1.x
                < layout.iter().find(|(w, _)| *w == 1).unwrap().1.x
        );
    }

    #[test]
    fn move_lone_root_window_is_a_no_op() {
        let mut tree = Tree::new();
        let only = insert(&mut tree, 1, None);
        assert!(!tree.move_in_direction(only, Direction::Left));
    }

    #[test]
    fn resize_weight_grows_target_and_shrinks_sibling() {
        let mut tree = Tree::new();
        let a = insert(&mut tree, 1, None);
        insert(&mut tree, 2, Some(a));

        let before = width_of(&tree.layout(area(), Gaps::default()), 1);
        assert!(tree.resize_weight(a, 0.2));
        let after = width_of(&tree.layout(area(), Gaps::default()), 1);
        assert!(after > before);
    }

    #[test]
    fn resize_weight_clamps_instead_of_going_negative() {
        let mut tree = Tree::new();
        let a = insert(&mut tree, 1, None);
        insert(&mut tree, 2, Some(a));

        assert!(tree.resize_weight(a, -100.0));
        let layout = tree.layout(area(), Gaps::default());
        for (_, rect) in &layout {
            assert!(rect.width > 0.0, "clamped weight must stay positive");
        }
    }

    #[test]
    fn resize_weight_never_crushes_a_smaller_sibling_below_min_weight() {
        // Regression test: `apply_resize` shrinks siblings by a uniform
        // *proportional* factor, so with uneven weights the smallest
        // sibling used to be crushed far below the documented floor even
        // though the aggregate bound looked fine — see `branch_resize_bounds`.
        let mut tree = Tree::new();
        let a = insert(&mut tree, 1, None);
        let b = insert(&mut tree, 2, Some(a));
        let c = insert(&mut tree, 3, Some(b));

        assert!(tree.resize_weight(c, -0.9));
        assert!(tree.resize_weight(a, 100.0));

        let parent = tree.parents[&a];
        let Some(Node::Container { weights, .. }) = tree.nodes.get(parent) else {
            panic!("expected a Tiles container");
        };
        const MIN_WEIGHT: f32 = 0.05; // mirrors `branch_resize_bounds`'s own constant
        for &w in weights {
            assert!(
                w >= MIN_WEIGHT,
                "sibling weight {w} fell below the documented floor {MIN_WEIGHT}"
            );
        }
    }

    #[test]
    fn resize_weight_with_no_tiles_ancestor_is_a_no_op() {
        let mut tree = Tree::new();
        let only = insert(&mut tree, 1, None);
        assert!(!tree.resize_weight(only, 0.1));
    }

    #[test]
    fn resize_handle_at_finds_the_boundary_between_two_siblings() {
        let mut tree = Tree::new();
        let a = insert(&mut tree, 1, None);
        let b = insert(&mut tree, 2, Some(a));

        // Equal-weight horizontal split of a 1000-wide area: boundary sits at x=500.
        let handle = tree
            .resize_handle_at(area(), Gaps::default(), (500.0, 400.0))
            .expect("point sits on the boundary");
        assert_eq!(handle.before, a);
        assert_eq!(handle.after, b);
        assert_eq!(handle.orientation, Orientation::Horizontal);
        assert!(handle.weight_per_pixel > 0.0);
    }

    #[test]
    fn resize_handle_at_away_from_any_boundary_is_none() {
        let mut tree = Tree::new();
        let a = insert(&mut tree, 1, None);
        insert(&mut tree, 2, Some(a));

        assert!(
            tree.resize_handle_at(area(), Gaps::default(), (100.0, 400.0))
                .is_none()
        );
    }

    #[test]
    fn resize_handle_at_on_a_lone_root_window_is_always_none() {
        let mut tree = Tree::new();
        insert(&mut tree, 1, None);

        assert!(
            tree.resize_handle_at(area(), Gaps::default(), (500.0, 400.0))
                .is_none()
        );
        assert!(
            tree.resize_handle_at(area(), Gaps::default(), (0.0, 0.0))
                .is_none()
        );
    }

    #[test]
    fn resize_handle_at_resolves_the_correct_nested_level() {
        // root(h) = [1, joined-container(v) = [2, 3]] — window 1's right
        // edge is the *root's* boundary, while 2/3's shared edge is the
        // nested container's own (vertical) boundary.
        let mut tree = Tree::new();
        let w1 = insert(&mut tree, 1, None);
        let w2 = insert(&mut tree, 2, Some(w1));
        let w3 = insert(&mut tree, 3, Some(w2));
        assert!(tree.join_with(w2, Direction::Right));

        let layout = tree.layout(area(), Gaps::default());
        let w1_rect = layout.iter().find(|(w, _)| *w == 1).unwrap().1;
        let w2_rect = layout.iter().find(|(w, _)| *w == 2).unwrap().1;

        let root_boundary = tree
            .resize_handle_at(area(), Gaps::default(), (w1_rect.x + w1_rect.width, 200.0))
            .expect("root-level horizontal boundary");
        assert_eq!(root_boundary.orientation, Orientation::Horizontal);
        assert_eq!(root_boundary.before, w1);

        let nested_boundary = tree
            .resize_handle_at(
                area(),
                Gaps::default(),
                (w2_rect.x + w2_rect.width / 2.0, w2_rect.y + w2_rect.height),
            )
            .expect("nested vertical boundary between 2 and 3");
        assert_eq!(nested_boundary.orientation, Orientation::Vertical);
        assert_eq!(nested_boundary.before, w2);
        assert_eq!(nested_boundary.after, w3);
    }

    #[test]
    fn resize_delta_bounds_matches_what_apply_resize_actually_clamps_to() {
        let mut probe = Tree::new();
        let probe_a = insert(&mut probe, 1, None);
        insert(&mut probe, 2, Some(probe_a));
        let (max_shrink, max_grow) = probe.resize_delta_bounds(probe_a).expect("has a sibling");
        assert!(max_shrink > 0.0);
        assert!(max_grow > 0.0);

        // Applying exactly the reported max_grow should reach (not exceed) the clamp — a bigger
        // delta must produce the same result as this exact one.
        let mut exact = Tree::new();
        let exact_a = insert(&mut exact, 1, None);
        insert(&mut exact, 2, Some(exact_a));
        assert!(exact.resize_weight(exact_a, max_grow));

        let mut over = Tree::new();
        let over_a = insert(&mut over, 1, None);
        insert(&mut over, 2, Some(over_a));
        assert!(over.resize_weight(over_a, max_grow + 10.0));

        assert_eq!(
            width_of(&exact.layout(area(), Gaps::default()), 1),
            width_of(&over.layout(area(), Gaps::default()), 1)
        );
    }

    #[test]
    fn resize_delta_bounds_is_none_with_no_tiles_ancestor() {
        let mut tree = Tree::new();
        let only = insert(&mut tree, 1, None);
        assert!(tree.resize_delta_bounds(only).is_none());
    }

    #[test]
    fn balance_weights_resets_parent_container_after_resize() {
        let mut tree = Tree::new();
        let a = insert(&mut tree, 1, None);
        insert(&mut tree, 2, Some(a));
        assert!(tree.resize_weight(a, 0.3));

        let skewed = width_of(&tree.layout(area(), Gaps::default()), 1);
        assert!(tree.balance_weights(a, false));
        let after = tree.layout(area(), Gaps::default());
        let w1 = width_of(&after, 1);
        let w2 = width_of(&after, 2);
        assert!((w1 - w2).abs() < 0.01, "balanced siblings are equal width");
        assert!(
            w1 < skewed,
            "resize had actually skewed w1 wider before balancing"
        );
    }

    #[test]
    fn balance_weights_root_targets_root_even_when_from_is_nested() {
        // root(h) = [1, joined-container(v) = [2, 3]] — the nested
        // container splits its two children by *height* (its own
        // orientation is Vertical), so both share whatever *width* the
        // root's split gives the nested branch as a whole.
        let mut tree = Tree::new();
        let w1 = insert(&mut tree, 1, None);
        let w2 = insert(&mut tree, 2, Some(w1));
        insert(&mut tree, 3, Some(w2));
        assert!(tree.join_with(w2, Direction::Right));
        assert!(tree.resize_weight(w1, 0.3));

        let skewed = tree.layout(area(), Gaps::default());
        assert!(
            (width_of(&skewed, 1) - width_of(&skewed, 2)).abs() > 1.0,
            "resize actually skewed the root split before balancing"
        );

        // `root: true` balances the outer root container (w1 vs the nested
        // pair), not w2's own immediate (nested) parent.
        assert!(tree.balance_weights(w2, true));
        let layout = tree.layout(area(), Gaps::default());
        let w1_width = width_of(&layout, 1);
        assert!((w1_width - width_of(&layout, 2)).abs() < 0.01);
        assert!((w1_width - width_of(&layout, 3)).abs() < 0.01);
    }

    #[test]
    fn balance_weights_does_not_touch_nested_containers_children() {
        let mut tree = Tree::new();
        let w1 = insert(&mut tree, 1, None);
        let w2 = insert(&mut tree, 2, Some(w1));
        insert(&mut tree, 3, Some(w2));
        assert!(tree.join_with(w2, Direction::Right)); // nests 2,3 under w2, vertically
        assert!(tree.resize_weight(w2, 0.3)); // skews the nested pair's heights, not the root

        let before = tree.layout(area(), Gaps::default());
        let w2_height_before = height_of(&before, 2);
        let w3_height_before = height_of(&before, 3);
        assert!(
            (w2_height_before - w3_height_before).abs() > 1.0,
            "resize actually skewed the nested pair"
        );

        // Balancing the root (from == w1, root == true) must not reset the
        // nested container's own weights.
        assert!(tree.balance_weights(w1, true));
        let after = tree.layout(area(), Gaps::default());
        assert_eq!(height_of(&after, 2), w2_height_before);
        assert_eq!(height_of(&after, 3), w3_height_before);
    }

    #[test]
    fn balance_weights_on_lone_root_window_is_a_no_op() {
        let mut tree = Tree::new();
        let only = insert(&mut tree, 1, None);
        assert!(!tree.balance_weights(only, false));
        assert!(!tree.balance_weights(only, true));
    }

    #[test]
    fn balance_weights_on_empty_tree_is_a_no_op() {
        let mut scratch = Tree::new();
        let stray = insert(&mut scratch, 1, None);

        let mut tree = Tree::new();
        assert!(!tree.balance_weights(stray, true));
        assert!(!tree.balance_weights(stray, false));
    }

    #[test]
    fn accordion_padding_insets_edges_only_for_first_and_last_of_three() {
        let mut tree = Tree::new();
        let w1 = insert(&mut tree, 1, None);
        let w2 = insert(&mut tree, 2, Some(w1));
        insert(&mut tree, 3, Some(w2)); // chained near, so children stay flat [1, 2, 3]
        assert!(tree.toggle_root_layout());

        let layout = tree.layout(
            area(),
            Gaps {
                accordion: 30.0,
                ..Gaps::default()
            },
        );
        assert_eq!(layout.len(), 3);
        let first = layout.iter().find(|(w, _)| *w == 1).unwrap().1;
        let last = layout.iter().find(|(w, _)| *w == 3).unwrap().1;

        // First child: only trailing padding (its own left edge is flush
        // with the container).
        assert_eq!(first.x, area().x);
        assert!(first.width < area().width);
        // Last child: only leading padding (its own right edge is flush).
        assert!((last.x + last.width - area().width).abs() < 0.01);
    }

    #[test]
    fn accordion_single_child_gets_no_padding() {
        let mut tree = Tree::new();
        insert(&mut tree, 1, None);
        // A lone root window has no container to toggle to Accordion, so
        // this exercises the "n == 1" branch of the accordion layout
        // itself via a 2-window tree collapsed back down isn't reachable —
        // instead verify directly that a single-child container renders
        // full-area regardless of padding by joining then removing one.
        let mut tree2 = Tree::new();
        let a = insert(&mut tree2, 10, None);
        insert(&mut tree2, 20, Some(a));
        assert!(tree2.toggle_root_layout());
        tree2.remove_window(20);
        let layout = tree2.layout(
            area(),
            Gaps {
                accordion: 30.0,
                ..Gaps::default()
            },
        );
        assert_eq!(layout, vec![(10, area())]);
        let _ = tree;
    }

    #[test]
    fn accordion_padding_zero_gives_every_child_the_full_frame() {
        let mut tree = Tree::new();
        let w1 = insert(&mut tree, 1, None);
        insert(&mut tree, 2, Some(w1));
        assert!(tree.toggle_root_layout());

        let layout = tree.layout(area(), Gaps::default());
        for (_, rect) in &layout {
            assert_eq!(*rect, area());
        }
    }

    #[test]
    fn remove_collapses_parent_and_leaves_sibling_as_bare_root() {
        let mut tree = Tree::new();
        let a = insert(&mut tree, 1, None);
        insert(&mut tree, 2, Some(a));

        let next_focus = tree.remove_window(1);
        assert_eq!(tree.window_ids(), vec![2]);
        assert_eq!(tree.window_at(next_focus.unwrap()), Some(2));
        assert_eq!(tree.window_at(tree.root().unwrap()), Some(2));
        assert_eq!(tree.orientation_of(tree.root().unwrap()), None);

        let layout = tree.layout(area(), Gaps::default());
        assert_eq!(layout, vec![(2, area())]);
    }

    #[test]
    fn remove_from_three_way_flat_container_keeps_the_other_two_flat() {
        let mut tree = Tree::new();
        let w1 = insert(&mut tree, 1, None);
        insert(&mut tree, 2, Some(w1));
        insert(&mut tree, 3, Some(w1));

        tree.remove_window(2);
        assert_eq!(tree.window_ids().len(), 2);
        let layout = tree.layout(area(), Gaps::default());
        let w1_width = width_of(&layout, 1);
        let w3_width = width_of(&layout, 3);
        assert!((w1_width - w3_width).abs() < 0.01);
    }

    #[test]
    fn remove_active_accordion_child_leaves_sibling_visible() {
        let mut tree = Tree::new();
        let a = insert(&mut tree, 1, None);
        insert(&mut tree, 2, Some(a));
        tree.record_focus(a);
        assert!(tree.toggle_root_layout());

        let next_focus = tree.remove_window(1);
        assert_eq!(tree.window_ids(), vec![2]);
        assert_eq!(tree.window_at(next_focus.unwrap()), Some(2));
        assert_eq!(tree.window_at(tree.root().unwrap()), Some(2));

        let layout = tree.layout(area(), Gaps::default());
        assert_eq!(layout, vec![(2, area())]);
    }

    #[test]
    fn remove_last_window_empties_the_tree() {
        let mut tree = Tree::new();
        insert(&mut tree, 1, None);
        let next_focus = tree.remove_window(1);
        assert_eq!(next_focus, None);
        assert!(tree.is_empty());
    }

    #[test]
    fn three_windows_navigate_correctly_when_flat() {
        let mut tree = Tree::new();
        let w1 = insert(&mut tree, 1, None);
        let w2 = insert(&mut tree, 2, Some(w1));
        let w3 = insert(&mut tree, 3, Some(w2));

        assert_eq!(tree.navigate(w1, Direction::Right), Some(w2));
        assert_eq!(tree.navigate(w2, Direction::Right), Some(w3));
        assert_eq!(tree.navigate(w3, Direction::Left), Some(w2));
        assert_eq!(tree.navigate(w2, Direction::Left), Some(w1));
        assert_eq!(tree.navigate(w3, Direction::Right), None);
        assert_eq!(tree.navigate(w1, Direction::Left), None);
    }

    #[test]
    fn floating_leaf_is_addressable_but_invisible_to_layout() {
        let mut tree = Tree::new();
        let w1 = insert(&mut tree, 1, None);
        let f = tree.insert_floating(2, Some(w1), Orientation::Horizontal);

        assert_eq!(tree.window_at(f), Some(2));
        assert_eq!(tree.find_node(2), Some(f));
        assert_eq!(tree.node_for_window(2), Some(f));
        assert_eq!(tree.window_ids(), vec![1, 2]);
        assert_eq!(tree.tiled_window_ids(), vec![1]);

        // The floating window gets no rect at all — the lone tiled sibling
        // fills the whole area, as if the floating one weren't there.
        let layout = tree.layout(area(), Gaps::default());
        assert_eq!(layout, vec![(1, area())]);
    }

    #[test]
    fn navigate_skips_over_a_floating_sibling() {
        let mut tree = Tree::new();
        let w1 = insert(&mut tree, 1, None);
        let w2 = insert(&mut tree, 2, Some(w1));
        // Insert the floating leaf between w1 and w2's shared container so
        // a naive index-based `navigate` would land on it instead of w2.
        tree.insert_floating(99, Some(w1), Orientation::Horizontal);

        assert_eq!(tree.navigate(w1, Direction::Right), Some(w2));
    }

    #[test]
    fn move_within_skips_over_a_floating_sibling() {
        let mut tree = Tree::new();
        let w1 = insert(&mut tree, 1, None);
        let w2 = insert(&mut tree, 2, Some(w1));
        tree.insert_floating(99, Some(w1), Orientation::Horizontal);

        assert!(tree.move_in_direction(w1, Direction::Right));
        assert_eq!(tree.window_at(w1), Some(1));
        assert_eq!(tree.window_at(w2), Some(2));
        // w1 and w2 swapped past the floating leaf; the floating window's
        // own placement in `children` is untouched — it still resolves to
        // the same window id.
        assert_eq!(tree.window_ids().len(), 3);
    }

    #[test]
    fn remove_window_works_for_a_floating_leaf() {
        let mut tree = Tree::new();
        let w1 = insert(&mut tree, 1, None);
        tree.insert_floating(2, Some(w1), Orientation::Horizontal);
        assert_eq!(tree.window_ids().len(), 2);

        let next_focus = tree.remove_window(2);
        assert_eq!(tree.window_at(next_focus.unwrap()), Some(1));
        assert_eq!(tree.window_ids(), vec![1]);
    }

    #[test]
    fn lone_floating_root_lays_out_to_nothing() {
        let mut tree = Tree::new();
        let f = tree.insert_floating(1, None, Orientation::Horizontal);
        assert_eq!(tree.root(), Some(f));
        assert_eq!(tree.window_at(f), Some(1));
        assert!(tree.layout(area(), Gaps::default()).is_empty());
        assert_eq!(tree.default_focus(), Some(f));
    }
}
