//! The pane split tree (§6 of the Phase 1 spec).
//!
//! ```text
//! Split
//! ├── Pane
//! └── Split
//!     ├── Pane
//!     └── Pane
//! ```
//!
//! Splits are binary (two children) with a `ratio` (0.0–1.0) for the first
//! child. The tree is plain data — no live sessions — and fully
//! serializable.

use serde::{Deserialize, Serialize};

use crate::model::{Pane, PaneId, SessionId, SplitDirection};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum PaneNode {
    Leaf(Pane),
    Split {
        direction: SplitDirection,
        /// Fraction of the parent rect given to the first child.
        ratio: f32,
        children: Box<[PaneNode; 2]>,
    },
}

impl PaneNode {
    pub fn leaf(pane: Pane) -> Self {
        PaneNode::Leaf(pane)
    }

    pub fn pane_id(&self) -> Option<&PaneId> {
        match self {
            PaneNode::Leaf(p) => Some(&p.id),
            PaneNode::Split { .. } => None,
        }
    }

    /// Depth-first pane iteration (stable order).
    pub fn panes<'a>(&'a self, out: &mut Vec<&'a Pane>) {
        match self {
            PaneNode::Leaf(p) => out.push(p),
            PaneNode::Split { children, .. } => {
                children[0].panes(out);
                children[1].panes(out);
            }
        }
    }

    pub fn panes_mut<'a>(&'a mut self, out: &mut Vec<&'a mut Pane>) {
        match self {
            PaneNode::Leaf(p) => out.push(p),
            PaneNode::Split { children, .. } => {
                let (c0, c1) = children.split_at_mut(1);
                c0[0].panes_mut(out);
                c1[0].panes_mut(out);
            }
        }
    }

    pub fn pane_count(&self) -> usize {
        match self {
            PaneNode::Leaf(_) => 1,
            PaneNode::Split { children, .. } => children[0].pane_count() + children[1].pane_count(),
        }
    }

    pub fn find_pane(&self, id: &PaneId) -> Option<&Pane> {
        match self {
            PaneNode::Leaf(p) if &p.id == id => Some(p),
            PaneNode::Split { children, .. } => children[0]
                .find_pane(id)
                .or_else(|| children[1].find_pane(id)),
            _ => None,
        }
    }

    pub fn find_pane_mut(&mut self, id: &PaneId) -> Option<&mut Pane> {
        match self {
            PaneNode::Leaf(p) if &p.id == id => Some(p),
            PaneNode::Leaf(_) => None,
            PaneNode::Split { children, .. } => {
                let (c0, c1) = children.split_at_mut(1);
                if let Some(p) = c0[0].find_pane_mut(id) {
                    return Some(p);
                }
                c1[0].find_pane_mut(id)
            }
        }
    }

    /// Splits the pane with `id` in `direction`, inserting `new_pane` as the
    /// second child. Returns the new pane's id (None if `id` was not found).
    pub fn split_by_id(
        &mut self,
        id: &PaneId,
        direction: SplitDirection,
        new_pane: Pane,
    ) -> Option<PaneId> {
        let new_id = new_pane.id.clone();
        match self {
            PaneNode::Leaf(p) if &p.id == id => {
                let old = std::mem::replace(p, Pane::new_terminal(SessionId::new(), ""));
                let mut old_leaf = PaneNode::Leaf(old);
                let _ = &mut old_leaf;
                *self = PaneNode::Split {
                    direction,
                    ratio: 0.5,
                    children: Box::new([old_leaf, PaneNode::Leaf(new_pane)]),
                };
                Some(new_id)
            }
            PaneNode::Leaf(_) => None,
            PaneNode::Split { children, .. } => {
                if let Some(id2) = children[0].split_by_id(id, direction, new_pane.clone()) {
                    return Some(id2);
                }
                children[1].split_by_id(id, direction, new_pane)
            }
        }
    }

    /// Removes the pane with `id`, collapsing its parent split if that would
    /// leave a single child. Returns the removed pane (if found).
    pub fn remove_pane(&mut self, id: &PaneId) -> Option<Pane> {
        match self {
            PaneNode::Leaf(p) if &p.id == id => {
                let old = p.clone();
                // Replace with an empty placeholder leaf; the parent
                // collapses it immediately after.
                *p = Pane::new_terminal(SessionId::new(), "");
                p.id = format!("__removed__{}", old.id);
                Some(old)
            }
            PaneNode::Leaf(_) => None,
            PaneNode::Split { children, .. } => {
                let removed = children[0]
                    .remove_pane(id)
                    .or_else(|| children[1].remove_pane(id));
                if removed.is_some() {
                    self.collapse_removed();
                }
                removed
            }
        }
    }

    /// Replaces any `__removed__` leaf with a live sibling by collapsing the
    /// parent split when one side is gone.
    fn collapse_removed(&mut self) {
        if let PaneNode::Split { children, .. } = self {
            let removed0 =
                matches!(&children[0], PaneNode::Leaf(p) if p.id.starts_with("__removed__"));
            let removed1 =
                matches!(&children[1], PaneNode::Leaf(p) if p.id.starts_with("__removed__"));
            if removed0 || removed1 {
                let keep = if removed0 {
                    std::mem::replace(
                        &mut children[1],
                        PaneNode::Leaf(Pane::new_terminal(SessionId::new(), "")),
                    )
                } else {
                    std::mem::replace(
                        &mut children[0],
                        PaneNode::Leaf(Pane::new_terminal(SessionId::new(), "")),
                    )
                };
                *self = keep;
            }
        }
    }

    /// Swaps the *contents* of two leaves in place (ids, sessions, titles,
    /// cwds). Returns true if both ids exist.
    pub fn swap_panes(&mut self, a: &PaneId, b: &PaneId) -> bool {
        if a == b {
            return true;
        }
        let mut both = Vec::new();
        self.panes_mut(&mut both);
        let mut pa = None;
        let mut pb = None;
        for p in both.drain(..) {
            if &p.id == a {
                pa = Some(p);
            } else if &p.id == b {
                pb = Some(p);
            }
        }
        match (pa, pb) {
            (Some(pa), Some(pb)) => {
                let ta = pa.title.clone();
                let ea = pa.execution_id.clone();
                let ka = pa.execution_kind;
                let ca = pa.cwd.clone();
                let ma = pa.metadata.clone();
                pa.title = pb.title.clone();
                pa.execution_id = pb.execution_id.clone();
                pa.execution_kind = pb.execution_kind;
                pa.cwd = pb.cwd.clone();
                pa.metadata = pb.metadata.clone();
                pb.title = ta;
                pb.execution_id = ea;
                pb.execution_kind = ka;
                pb.cwd = ca;
                pb.metadata = ma;
                true
            }
            _ => false,
        }
    }

    /// Moves pane `id` one step in the given direction within its parent
    /// split (left/up = index 0; right/down = index 1). Returns true if moved.
    pub fn move_pane(&mut self, id: &PaneId, forward: bool) -> bool {
        match self {
            PaneNode::Leaf(_) => false,
            PaneNode::Split { children, .. } => {
                if children[0].move_pane(id, forward) || children[1].move_pane(id, forward) {
                    return true;
                }
                // Direct children: try to swap with the sibling.
                let in0 = children[0].pane_id().is_some() && children[0].pane_id() == Some(id);
                let in1 = children[1].pane_id().is_some() && children[1].pane_id() == Some(id);
                if in0 && forward {
                    children.swap(0, 1);
                    return true;
                }
                if in1 && !forward {
                    children.swap(0, 1);
                    return true;
                }
                false
            }
        }
    }
}

impl Default for PaneNode {
    fn default() -> Self {
        PaneNode::Leaf(Pane::new_terminal(SessionId::new(), ""))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{new_id, SplitDirection::*};
    use terminal_session::execution::ExecutionId;

    fn pane() -> Pane {
        Pane::new_terminal(new_id(), "/tmp")
    }

    #[test]
    fn split_creates_two_leaves() {
        let mut root = PaneNode::leaf(pane());
        let root_id = root.pane_id().unwrap().clone();
        let new_pane = pane();
        let new_id = new_pane.id.clone();
        let inserted = root.split_by_id(&root_id, Horizontal, new_pane);
        assert_eq!(inserted.as_deref(), Some(new_id.as_str()));
        assert_eq!(root.pane_count(), 2);
        assert!(root.find_pane(&root_id).is_some());
        assert!(root.find_pane(&new_id).is_some());
    }

    #[test]
    fn split_deep_tree() {
        let mut root = PaneNode::leaf(pane());
        let a = root.pane_id().unwrap().clone();
        let p1 = pane();
        let p1id = p1.id.clone();
        root.split_by_id(&a, Horizontal, p1);
        let b = root.find_pane(&p1id).unwrap().id.clone();
        let p2 = pane();
        let p2id = p2.id.clone();
        root.split_by_id(&b, Vertical, p2);
        assert_eq!(root.pane_count(), 3);
        assert!(root.find_pane(&p2id).is_some());
    }

    #[test]
    fn remove_collapses_parent() {
        let mut root = PaneNode::leaf(pane());
        let a = root.pane_id().unwrap().clone();
        let p1 = pane();
        let p1id = p1.id.clone();
        root.split_by_id(&a, Horizontal, p1);
        assert_eq!(root.pane_count(), 2);
        let removed = root.remove_pane(&a);
        assert!(removed.is_some());
        assert_eq!(root.pane_count(), 1);
        assert!(root.find_pane(&p1id).is_some());
    }

    #[test]
    fn swap_exchanges_sessions() {
        let mut root = PaneNode::leaf(pane());
        let a = root.pane_id().unwrap().clone();
        let p1 = pane();
        let p1id = p1.id.clone();
        let p1_eid = p1.execution_id.clone();
        let a_eid = root.find_pane(&a).unwrap().execution_id.clone();
        root.split_by_id(&a, Horizontal, p1);
        assert!(root.swap_panes(&a, &p1id));
        // After the swap, pane `a` hosts the execution that pane p1 had.
        assert_eq!(root.find_pane(&a).unwrap().execution_id, p1_eid);
        assert_eq!(root.find_pane(&p1id).unwrap().execution_id, a_eid);
        let _ = ExecutionId::new();
    }

    #[test]
    fn serialize_roundtrip() {
        let mut root = PaneNode::leaf(pane());
        let a = root.pane_id().unwrap().clone();
        let p1 = pane();
        root.split_by_id(&a, Vertical, p1);
        let json = serde_json::to_string(&root).unwrap();
        let back: PaneNode = serde_json::from_str(&json).unwrap();
        assert_eq!(back, root);
    }
}
