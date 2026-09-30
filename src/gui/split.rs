//! The pane arrangement tree.
//!
//! Auto mode (no tree) keeps the classic arrangement: the agent-anchored main
//! pane on the left, everything else stacked on the side, shell panes along
//! the bottom. The first manual body-drop ("split this pane in two") plants a
//! tree, and from then on the arrangement is whatever the tree says — panes
//! divide where you drop, panes that go away are pruned, and a tree that
//! collapses to a single pane switches back to auto.
//!
//! The logic here is pure data — no widgets — so the tree ops are testable
//! without a display.

use std::rc::Rc;

/// Which way a split divides the space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    Horizontal,
    Vertical,
}

/// One node of the arrangement: a pane, or a divider with two sides.
pub enum Node<L> {
    Leaf(Rc<L>),
    Split {
        axis: Axis,
        /// The first side's share, 0.0..1.0.
        ratio: f64,
        /// Stable key for divider-position memory.
        key: String,
        first: Box<Node<L>>,
        second: Box<Node<L>>,
    },
}

impl<L> Clone for Node<L> {
    fn clone(&self) -> Self {
        match self {
            Node::Leaf(panel) => Node::Leaf(Rc::clone(panel)),
            Node::Split {
                axis,
                ratio,
                key,
                first,
                second,
            } => Node::Split {
                axis: *axis,
                ratio: *ratio,
                key: key.clone(),
                first: Box::new((**first).clone()),
                second: Box::new((**second).clone()),
            },
        }
    }
}

impl<L> Node<L> {
    pub fn leaf(panel: &Rc<L>) -> Node<L> {
        Node::Leaf(Rc::clone(panel))
    }

    pub fn split(
        axis: Axis,
        ratio: f64,
        key: impl Into<String>,
        first: Node<L>,
        second: Node<L>,
    ) -> Node<L> {
        Node::Split {
            axis,
            ratio,
            key: key.into(),
            first: Box::new(first),
            second: Box::new(second),
        }
    }

    /// Does this subtree hold `panel`?
    pub fn contains(&self, panel: &Rc<L>) -> bool {
        match self {
            Node::Leaf(g) => Rc::ptr_eq(g, panel),
            Node::Split { first, second, .. } => first.contains(panel) || second.contains(panel),
        }
    }

    /// Every leaf, in layout order.
    pub fn leaves(&self) -> Vec<Rc<L>> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect(&self, out: &mut Vec<Rc<L>>) {
        match self {
            Node::Leaf(g) => out.push(Rc::clone(g)),
            Node::Split { first, second, .. } => {
                first.collect(out);
                second.collect(out);
            }
        }
    }

    /// Replace the leaf holding `target` with `with`. False when `target` is
    /// not in this subtree.
    pub fn replace(&mut self, target: &Rc<L>, with: Node<L>) -> bool {
        let mut with = Some(with);
        fn walk<L>(node: &mut Node<L>, target: &Rc<L>, with: &mut Option<Node<L>>) -> bool {
            match node {
                Node::Leaf(g) if Rc::ptr_eq(g, target) => {
                    *node = with.take().expect("a leaf can appear only once in a tree");
                    true
                }
                Node::Leaf(_) => false,
                Node::Split { first, second, .. } => {
                    walk(first, target, with) || walk(second, target, with)
                }
            }
        }
        walk(self, target, &mut with)
    }

    /// Exchange the positions of two leaves without changing the split tree.
    pub fn swap_leaves(&mut self, first: &Rc<L>, second: &Rc<L>) -> bool {
        if Rc::ptr_eq(first, second) || !self.contains(first) || !self.contains(second) {
            return false;
        }

        fn walk<L>(node: &mut Node<L>, first: &Rc<L>, second: &Rc<L>) {
            match node {
                Node::Leaf(panel) if Rc::ptr_eq(panel, first) => {
                    *panel = Rc::clone(second);
                }
                Node::Leaf(panel) if Rc::ptr_eq(panel, second) => {
                    *panel = Rc::clone(first);
                }
                Node::Leaf(_) => {}
                Node::Split {
                    first: a,
                    second: b,
                    ..
                } => {
                    walk(a, first, second);
                    walk(b, first, second);
                }
            }
        }

        walk(self, first, second);
        true
    }

    /// Drop leaves whose panel is gone, hoisting the survivor when one side of
    /// a split dies. None when the whole subtree is gone.
    pub fn prune(self, alive: &[Rc<L>]) -> Option<Node<L>> {
        match self {
            Node::Leaf(g) => alive
                .iter()
                .any(|a| Rc::ptr_eq(a, &g))
                .then(|| Node::Leaf(g)),
            Node::Split {
                axis,
                ratio,
                key,
                first,
                second,
            } => match (first.prune(alive), second.prune(alive)) {
                (Some(f), Some(s)) => Some(Node::split(axis, ratio, key, f, s)),
                (Some(f), None) => Some(f),
                (None, Some(s)) => Some(s),
                (None, None) => None,
            },
        }
    }

    /// Take the leaf holding `panel` out of the tree, hoisting the surviving
    /// side into its place. None when the leaf was the whole tree — or when
    /// the panel was not here at all.
    pub fn take_leaf(self, panel: &Rc<L>) -> Option<Node<L>> {
        match self {
            Node::Leaf(g) => (!Rc::ptr_eq(&g, panel)).then(|| Node::Leaf(g)),
            Node::Split {
                axis,
                ratio,
                key,
                first,
                second,
            } => match (first.take_leaf(panel), second.take_leaf(panel)) {
                (Some(f), Some(s)) => Some(Node::split(axis, ratio, key, f, s)),
                (Some(f), None) => Some(f),
                (None, Some(s)) => Some(s),
                (None, None) => None,
            },
        }
    }

    /// Add a pane beside the deepest last leaf: where brand-new panes land in
    /// an arranged tree.
    pub fn append(&mut self, panel: &Rc<L>) {
        match self {
            Node::Leaf(existing) => {
                let existing = Rc::clone(existing);
                *self = Node::split(
                    Axis::Horizontal,
                    0.7,
                    "tree-append",
                    Node::leaf(&existing),
                    Node::leaf(panel),
                );
            }
            Node::Split { second, .. } => second.append(panel),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (1 | 2) / 3 — a horizontal split of 1 and 2, vertical with 3 below.
    /// The handles come back so tests can refer to the very same leaves.
    fn tree_with() -> (Node<usize>, Vec<Rc<usize>>) {
        let one = Rc::new(1);
        let two = Rc::new(2);
        let three = Rc::new(3);
        let t = Node::split(
            Axis::Vertical,
            0.68,
            "outer",
            Node::split(
                Axis::Horizontal,
                0.42,
                "main",
                Node::leaf(&one),
                Node::leaf(&two),
            ),
            Node::leaf(&three),
        );
        (t, vec![one, two, three])
    }

    fn ids(t: &Node<usize>) -> Vec<usize> {
        t.leaves().iter().map(|g| **g).collect()
    }

    #[test]
    fn collect_walks_layout_order() {
        let (t, _) = tree_with();
        assert_eq!(ids(&t), vec![1, 2, 3]);
    }

    #[test]
    fn replace_swaps_only_the_target_leaf() {
        let (mut t, handles) = tree_with();
        let replacement = Rc::new(9);
        assert!(t.replace(&handles[1], Node::leaf(&replacement)));
        assert_eq!(ids(&t), vec![1, 9, 3]);
        let stranger = Rc::new(42);
        assert!(
            !t.replace(&stranger, Node::leaf(&Rc::new(0))),
            "absent target"
        );
    }

    #[test]
    fn swap_leaves_moves_panels_without_changing_the_split_shape() {
        let (mut t, handles) = tree_with();
        assert!(t.swap_leaves(&handles[0], &handles[2]));
        assert_eq!(ids(&t), vec![3, 2, 1]);
        assert_eq!(t.leaves().len(), 3);
    }

    #[test]
    fn swap_leaves_does_nothing_when_a_panel_is_missing() {
        let (mut t, handles) = tree_with();
        let missing = Rc::new(4);
        assert!(!t.swap_leaves(&handles[0], &missing));
        assert_eq!(ids(&t), vec![1, 2, 3]);
    }

    #[test]
    fn prune_hoists_the_survivor() {
        let (t, handles) = tree_with();
        let alive = vec![Rc::clone(&handles[0]), Rc::clone(&handles[2])];
        let t = t.prune(&alive).expect("two leaves survive");
        assert_eq!(ids(&t), vec![1, 3]);
    }

    #[test]
    fn prune_of_a_dead_whole_is_none() {
        let alive = vec![Rc::new(99)];
        assert!(tree_with().0.prune(&alive).is_none());
    }

    #[test]
    fn prune_keeps_everything_when_all_alive() {
        let (t, handles) = tree_with();
        let t = t.prune(&handles).expect("all survive");
        assert_eq!(t.leaves().len(), 3);
    }

    #[test]
    fn append_goes_beside_the_deepest_last_leaf() {
        let (mut t, _) = tree_with();
        let four = Rc::new(4);
        t.append(&four);
        assert_eq!(ids(&t), vec![1, 2, 3, 4]);
    }

    #[test]
    fn take_leaf_hoists_the_survivor() {
        let (t, handles) = tree_with();
        let t = t.take_leaf(&handles[1]).expect("two leaves remain");
        assert_eq!(ids(&t), vec![1, 3]);
    }

    #[test]
    fn take_leaf_of_the_whole_tree_is_none() {
        let one = Rc::new(1);
        assert!(Node::leaf(&one).take_leaf(&one).is_none());
    }

    #[test]
    fn taking_an_absent_leaf_leaves_the_tree_be() {
        let (t, _) = tree_with();
        let stranger = Rc::new(7);
        let t = t.take_leaf(&stranger).expect("untouched");
        assert_eq!(ids(&t), vec![1, 2, 3]);
    }

    #[test]
    fn contains_finds_deep_leaves() {
        let (t, handles) = tree_with();
        assert!(t.contains(&handles[1]));
        let stranger = Rc::new(7);
        assert!(!t.contains(&stranger));
    }
}
