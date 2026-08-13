//! Layout engine (§14): converts a pane tree into pane rectangles.
//!
//! Splits are binary with a `ratio` for the first child; rectangles are
//! computed recursively in a single pass. Cost is O(pane count) with no
//! allocation beyond the output vector.

use crate::model::{PaneId, SplitDirection};
use crate::pane_tree::PaneNode;

/// Integer pixel rect (origin top-left).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

impl Rect {
    pub fn contains(&self, px: f64, py: f64) -> bool {
        px >= self.x as f64
            && py >= self.y as f64
            && px < (self.x + self.width as i32) as f64
            && py < (self.y + self.height as i32) as f64
    }
}

/// A computed pane position for the current frame.
#[derive(Debug, Clone, PartialEq)]
pub struct PaneRect {
    pub pane_id: PaneId,
    pub rect: Rect,
}

/// Minimum pane size (px) along the split axis; layouts below this clamp to
/// the minimum rather than producing zero-width panes.
pub const MIN_PANE_PX: u32 = 40;

#[derive(Debug, Clone, Default)]
pub struct LayoutEngine;

impl LayoutEngine {
    pub fn new() -> Self {
        Self
    }

    /// Computes rectangles for every pane in `root` inside `outer`.
    /// If `zoom` is Some(pane_id), that pane receives the full rect.
    pub fn layout(&self, root: &PaneNode, outer: Rect, zoom: Option<&PaneId>) -> Vec<PaneRect> {
        let mut out = Vec::with_capacity(root.pane_count());
        if let Some(z) = zoom {
            if root.find_pane(z).is_some() {
                self.walk(root, outer, &mut out, Some(z));
                return out;
            }
        }
        self.walk(root, outer, &mut out, None);
        out
    }

    fn walk(&self, node: &PaneNode, rect: Rect, out: &mut Vec<PaneRect>, zoom: Option<&PaneId>) {
        match node {
            PaneNode::Leaf(p) => {
                // When zoomed we only emit the zoomed pane (caller handles
                // full rect); otherwise normal.
                if let Some(z) = zoom {
                    if &p.id == z {
                        out.push(PaneRect {
                            pane_id: p.id.clone(),
                            rect,
                        });
                    }
                } else {
                    out.push(PaneRect {
                        pane_id: p.id.clone(),
                        rect,
                    });
                }
            }
            PaneNode::Split {
                direction,
                ratio,
                children,
            } => {
                if zoom.is_some() {
                    // Zoomed: the whole rect belongs to the zoomed pane; the
                    // leaf filter below emits only it (and never split rects).
                    self.walk(&children[0], rect, out, zoom);
                    self.walk(&children[1], rect, out, zoom);
                } else {
                    let r = ratio.clamp(0.0, 1.0);
                    let (a, b) = self.split_rect(rect, *direction, r);
                    self.walk(&children[0], a, out, zoom);
                    self.walk(&children[1], b, out, zoom);
                }
            }
        }
    }

    fn split_rect(&self, rect: Rect, direction: SplitDirection, ratio: f32) -> (Rect, Rect) {
        match direction {
            SplitDirection::Horizontal => {
                let min_w = MIN_PANE_PX.min(rect.width / 2).max(1);
                let avail = rect.width.saturating_sub(min_w);
                let first_w = (rect.width as f32 * ratio) as u32;
                let first_w = first_w.clamp(min_w, avail.max(min_w));
                let second_w = rect.width.saturating_sub(first_w);
                (
                    Rect {
                        x: rect.x,
                        y: rect.y,
                        width: first_w,
                        height: rect.height,
                    },
                    Rect {
                        x: rect.x + first_w as i32,
                        y: rect.y,
                        width: second_w,
                        height: rect.height,
                    },
                )
            }
            SplitDirection::Vertical => {
                let min_h = MIN_PANE_PX.min(rect.height / 2).max(1);
                let avail = rect.height.saturating_sub(min_h);
                let first_h = (rect.height as f32 * ratio) as u32;
                let first_h = first_h.clamp(min_h, avail.max(min_h));
                let second_h = rect.height.saturating_sub(first_h);
                (
                    Rect {
                        x: rect.x,
                        y: rect.y,
                        width: rect.width,
                        height: first_h,
                    },
                    Rect {
                        x: rect.x,
                        y: rect.y + first_h as i32,
                        width: rect.width,
                        height: second_h,
                    },
                )
            }
        }
    }

    /// Resizes the split *containing* `pane_id` by `delta_px` along the
    /// split axis (positive grows the pane, shrinking its sibling). Returns
    /// true if the pane was found. `delta_px` is in ratio-space units
    /// (0.001 per px of a typical 1000-px axis).
    pub fn resize_pane(&self, root: &mut PaneNode, pane_id: &PaneId, delta_px: f32) -> bool {
        Self::resize_pane_inner(root, pane_id, delta_px)
    }

    fn resize_pane_inner(root: &mut PaneNode, pane_id: &PaneId, delta_px: f32) -> bool {
        match root {
            PaneNode::Leaf(_) => false,
            PaneNode::Split {
                ratio, children, ..
            } => {
                let in0 = children[0].find_pane(pane_id).is_some();
                let in1 = children[1].find_pane(pane_id).is_some();
                if in0 && !in1 {
                    *ratio = (*ratio + delta_px / 1000.0).clamp(0.05, 0.95);
                    true
                } else if in1 && !in0 {
                    *ratio = (*ratio - delta_px / 1000.0).clamp(0.05, 0.95);
                    true
                } else {
                    Self::resize_pane_inner(&mut children[0], pane_id, delta_px)
                        || Self::resize_pane_inner(&mut children[1], pane_id, delta_px)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{new_id, SplitDirection::*};
    use crate::pane_tree::PaneNode;

    fn pane() -> crate::model::Pane {
        crate::model::Pane::new_terminal(new_id(), "/tmp")
    }

    #[test]
    fn single_pane_full_rect() {
        let root = PaneNode::leaf(pane());
        let outer = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 60,
        };
        let out = LayoutEngine::new().layout(&root, outer, None);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rect, outer);
    }

    #[test]
    fn horizontal_split_side_by_side() {
        let mut root = PaneNode::leaf(pane());
        let a = root.pane_id().unwrap().clone();
        let p1 = pane();
        root.split_by_id(&a, Horizontal, p1);
        let outer = Rect {
            x: 0,
            y: 0,
            width: 200,
            height: 60,
        };
        let out = LayoutEngine::new().layout(&root, outer, None);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].rect.x, 0);
        assert_eq!(out[1].rect.x, 100);
        assert_eq!(out[0].rect.width + out[1].rect.width, 200);
        assert_eq!(out[0].rect.height, 60);
    }

    #[test]
    fn vertical_split_stacked() {
        let mut root = PaneNode::leaf(pane());
        let a = root.pane_id().unwrap().clone();
        let p1 = pane();
        root.split_by_id(&a, Vertical, p1);
        let outer = Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 200,
        };
        let out = LayoutEngine::new().layout(&root, outer, None);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].rect.y, 0);
        assert_eq!(out[1].rect.y, 100);
        assert_eq!(out[0].rect.height + out[1].rect.height, 200);
    }

    #[test]
    fn zoom_gives_full_rect() {
        let mut root = PaneNode::leaf(pane());
        let a = root.pane_id().unwrap().clone();
        let p1 = pane();
        let p1id = p1.id.clone();
        root.split_by_id(&a, Horizontal, p1);
        let outer = Rect {
            x: 0,
            y: 0,
            width: 200,
            height: 60,
        };
        let out = LayoutEngine::new().layout(&root, outer, Some(&p1id));
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].pane_id, p1id);
        assert_eq!(out[0].rect, outer);
    }

    #[test]
    fn resize_ratio_changes() {
        let mut root = PaneNode::leaf(pane());
        let a = root.pane_id().unwrap().clone();
        let p1 = pane();
        root.split_by_id(&a, Horizontal, p1);
        assert!(LayoutEngine::new().resize_pane(&mut root, &a, 100.0));
        if let PaneNode::Split { ratio, .. } = &root {
            assert!(*ratio > 0.5);
        } else {
            panic!("expected split");
        }
    }
}
