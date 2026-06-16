//! Implicit timeline tree: immutable, duration-weighted AVL with structural sharing.
//!
//! Generic over the element type via [`Tilable`]; instantiated as
//! `ImplicitTimelineTree<Turn>` on speech tracks and `ImplicitTimelineTree<Label>`
//! on track 0. Edits path-copy to the root via `Arc<Node<T>>`, leaving prior roots
//! intact for snapshot and undo. See
//! [data-model.md § Implicit timeline tree](data-model.md#implicit-timeline-tree).

use std::sync::Arc;

use super::hash::Hash;
use super::tilable::Tilable;
use super::turn::Turn;

/// Errors returned by tree mutations.
#[derive(Debug)]
pub enum TreeError {
    /// `sample` is negative or outside the operation's valid range.
    ///
    /// For [`ImplicitTimelineTree::insert_at`]: valid range is `[0, total_duration()]`.
    /// For [`ImplicitTimelineTree::update_at`] / [`ImplicitTimelineTree::delete_at`]:
    /// valid range is `[0, total_duration())`.
    SampleOutOfRange(i64),
    /// `insert_at` requires `sample` to be 0, an element boundary, or `total_duration()`.
    ///
    /// The provided sample fell inside an element's interior; `in_element_offset` is
    /// the offset from that element's start sample (always `> 0` and `< element.total_duration()`).
    SampleNotOnBoundary {
        /// The sample passed to `insert_at`.
        sample: i64,
        /// Offset within the element that contains `sample`.
        in_element_offset: i64,
    },
}

impl std::fmt::Display for TreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TreeError::SampleOutOfRange(s) => write!(f, "sample {s} is out of range"),
            TreeError::SampleNotOnBoundary {
                sample,
                in_element_offset,
            } => write!(
                f,
                "sample {sample} is not on an element boundary \
                 (in-element offset: {in_element_offset})"
            ),
        }
    }
}

impl std::error::Error for TreeError {}

/// A hit returned by [`ImplicitTimelineTree::element_at_sample`].
///
/// Carries the element's hash, the shared `Arc` to the element payload, the
/// in-element offset (interpretation per element kind — see
/// [data-model.md § Temporal query](data-model.md#temporal-query)), and the
/// hash of the element immediately preceding the hit (`None` if the hit is
/// the first element on the track).
#[derive(Debug, Clone)]
pub struct ElementHit<T> {
    /// Hash of the hit element.
    pub hash: Hash,
    /// Shared pointer to the hit element.
    pub element: Arc<T>,
    /// Offset within the element, in project-rate samples.
    ///
    /// Non-negative and `< element.total_duration()`. Per-kind interpretation
    /// (in-speech vs. post-turn silence vs. inter-label gap) is the caller's job.
    pub in_offset: i64,
    /// Hash of the element immediately before the hit; `None` if this is the first element.
    pub predecessor: Option<Hash>,
}

/// In-order iterator item: an element with its computed start sample.
#[derive(Debug, Clone, Copy)]
pub struct ElementRef<'a, T> {
    /// Project-rate sample at which this element begins.
    pub start_sample: i64,
    /// Content hash of the element.
    pub hash: Hash,
    /// Shared pointer to the element payload (lifetime-bound to the tree).
    pub element: &'a Arc<T>,
}

/// Stack-based in-order iterator over an [`ImplicitTimelineTree`].
///
/// Yields [`ElementRef`] items in timeline order with running start samples.
/// Produced by [`ImplicitTimelineTree::iter`].
pub struct TreeIter<'a, T: Tilable> {
    /// Pending nodes with their subtree-base offsets (sample at which the subtree starts).
    stack: Vec<(&'a Node<T>, i64)>,
}

/// In-order iterator item yielded by [`OwnedTreeIter`].
#[derive(Clone)]
pub struct OwnedElementRef<T> {
    /// Project-rate sample at which this element begins.
    pub start_sample: i64,
    /// Content hash of the element.
    pub hash: Hash,
    /// Shared pointer to the element payload.
    pub element: Arc<T>,
}

/// Stack-based in-order iterator that owns its traversal state via `Arc<Node<T>>`.
///
/// Unlike [`TreeIter`], this iterator holds no lifetime-bound references to the tree,
/// making it `'static + Send` as long as `T: Send + Sync + 'static`. Produced by
/// [`ImplicitTimelineTree::owned_iter_from`].
pub struct OwnedTreeIter<T: Tilable> {
    stack: Vec<(Arc<Node<T>>, i64)>,
}

impl<T: Tilable> Iterator for OwnedTreeIter<T> {
    type Item = OwnedElementRef<T>;

    fn next(&mut self) -> Option<Self::Item> {
        let (node, base) = self.stack.pop()?;
        let start_sample = base + node.left_subtree_sum;
        let right_base = start_sample + node.element.total_duration();
        // Push left spine of right subtree with owned Arc clones.
        let mut cur = node.right.clone();
        while let Some(r) = cur {
            let left = r.left.clone();
            self.stack.push((r, right_base));
            cur = left;
        }
        Some(OwnedElementRef {
            start_sample,
            hash: node.hash,
            element: Arc::clone(&node.element),
        })
    }
}

impl<'a, T: Tilable> Iterator for TreeIter<'a, T> {
    type Item = ElementRef<'a, T>;

    fn next(&mut self) -> Option<Self::Item> {
        let (node, base) = self.stack.pop()?;
        let start_sample = base + node.left_subtree_sum;
        // Push left-spine of right subtree onto the stack.
        let right_base = start_sample + node.element.total_duration();
        let mut cur = node.right.as_deref();
        while let Some(r) = cur {
            self.stack.push((r, right_base));
            cur = r.left.as_deref();
        }
        Some(ElementRef {
            start_sample,
            hash: node.hash,
            element: &node.element,
        })
    }
}

// --- merged turn iterator (multi-track transcript) ---

/// Lazy k-way merge over several turn trees, yielding `(start_sample, end_sample, &Turn)` in
/// global timeline order — the turn-level analog of [`crate::audio::edl::EdlCursor`].
///
/// Each entry is `(project_start_sample, &tree)`: a track's turns sit on the project timeline at
/// `project_start_sample + tree-local start_sample`, so tracks that begin at different project
/// offsets still interleave in true global order (mirroring how [`crate::audio::edl::EdlCursor`]
/// offsets each track). Each per-tree [`TreeIter`] is already start-ordered, so a single pass
/// picking the smallest peeked project position (ties broken by track order for determinism)
/// emits the globally-next turn without materialising or sorting. `end_sample = start_sample +
/// turn_duration` (the speech span, excluding trailing silence). A single tree degenerates to one
/// peekable iterator (i.e. `tree.iter()`), so callers need no separate single-track path.
pub struct MergedTurns<'a> {
    /// Per-track `(project_start_sample, peekable in-order iterator)`.
    iters: Vec<(i64, std::iter::Peekable<TreeIter<'a, Turn>>)>,
}

impl<'a> MergedTurns<'a> {
    /// Merge the given turn trees in global timeline order.
    ///
    /// Each entry is `(project_start_sample, &tree)`; turns are positioned at
    /// `project_start_sample + tree-local start_sample`.
    pub fn new(trees: &[(i64, &'a ImplicitTimelineTree<Turn>)]) -> Self {
        Self {
            iters: trees
                .iter()
                .map(|(offset, t)| (*offset, t.iter().peekable()))
                .collect(),
        }
    }
}

impl<'a> Iterator for MergedTurns<'a> {
    type Item = (i64, i64, &'a Turn);

    fn next(&mut self) -> Option<Self::Item> {
        // Pick the iterator whose next turn starts earliest in project time (tree-local start plus
        // the track's project offset); strict `<` keeps ties on the lower-indexed track, making
        // the order deterministic.
        let mut best: Option<usize> = None;
        let mut best_start = i64::MAX;
        for (i, (offset, it)) in self.iters.iter_mut().enumerate() {
            if let Some(e) = it.peek() {
                let start = *offset + e.start_sample;
                if start < best_start {
                    best_start = start;
                    best = Some(i);
                }
            }
        }
        // `best` came from a successful `peek`, so `next` yields the same element; `?` keeps this
        // panic-free without an `expect`.
        let (offset, it) = &mut self.iters[best?];
        let e = it.next()?;
        let start = *offset + e.start_sample;
        let end = start + e.element.turn_duration;
        Some((start, end, e.element.as_ref()))
    }
}

// --- private node type ---

struct Node<T: Tilable> {
    hash: Hash,
    element: Arc<T>,
    left: Option<Arc<Node<T>>>,
    right: Option<Arc<Node<T>>>,
    /// Σ `total_duration()` over the left subtree. Derived; never serialized.
    left_subtree_sum: i64,
    /// Σ `total_duration()` over the entire subtree. Derived; never serialized.
    total_subtree_sum: i64,
    /// AVL height (1 for a leaf). Derived; never serialized.
    height: u8,
}

impl<T: Tilable> Node<T> {
    fn leaf(hash: Hash, element: Arc<T>) -> Arc<Self> {
        let dur = element.total_duration();
        Arc::new(Node {
            left_subtree_sum: 0,
            total_subtree_sum: dur,
            height: 1,
            hash,
            element,
            left: None,
            right: None,
        })
    }

    fn make(
        left: Option<Arc<Node<T>>>,
        hash: Hash,
        element: Arc<T>,
        right: Option<Arc<Node<T>>>,
    ) -> Arc<Self> {
        let left_sum = left.as_ref().map_or(0, |l| l.total_subtree_sum);
        let right_sum = right.as_ref().map_or(0, |r| r.total_subtree_sum);
        let lh = left.as_ref().map_or(0u8, |l| l.height);
        let rh = right.as_ref().map_or(0u8, |r| r.height);
        Arc::new(Node {
            left_subtree_sum: left_sum,
            total_subtree_sum: left_sum + element.total_duration() + right_sum,
            height: 1 + lh.max(rh),
            hash,
            element,
            left,
            right,
        })
    }
}

// --- AVL rotation helpers ---

fn rotate_right<T: Tilable>(
    left: Arc<Node<T>>,
    hash: Hash,
    element: Arc<T>,
    right: Option<Arc<Node<T>>>,
) -> Arc<Node<T>> {
    let new_right = Node::make(left.right.clone(), hash, element, right);
    Node::make(
        left.left.clone(),
        left.hash,
        Arc::clone(&left.element),
        Some(new_right),
    )
}

fn rotate_left<T: Tilable>(
    left: Option<Arc<Node<T>>>,
    hash: Hash,
    element: Arc<T>,
    right: Arc<Node<T>>,
) -> Arc<Node<T>> {
    let new_left = Node::make(left, hash, element, right.left.clone());
    Node::make(
        Some(new_left),
        right.hash,
        Arc::clone(&right.element),
        right.right.clone(),
    )
}

fn rebalance_left<T: Tilable>(
    left: Arc<Node<T>>,
    hash: Hash,
    element: Arc<T>,
    right: Option<Arc<Node<T>>>,
) -> Arc<Node<T>> {
    let l_lh = left.left.as_ref().map_or(0u8, |l| l.height);
    let l_rh = left.right.as_ref().map_or(0u8, |r| r.height);
    if l_lh >= l_rh {
        // Left-left: single right rotation.
        rotate_right(left, hash, element, right)
    } else {
        // Left-right: rotate left child left, then rotate whole right.
        match &left.right {
            Some(lr) => {
                let new_left = rotate_left(
                    left.left.clone(),
                    left.hash,
                    Arc::clone(&left.element),
                    Arc::clone(lr),
                );
                rotate_right(new_left, hash, element, right)
            }
            None => Node::make(Some(left), hash, element, right),
        }
    }
}

fn rebalance_right<T: Tilable>(
    left: Option<Arc<Node<T>>>,
    hash: Hash,
    element: Arc<T>,
    right: Arc<Node<T>>,
) -> Arc<Node<T>> {
    let r_lh = right.left.as_ref().map_or(0u8, |l| l.height);
    let r_rh = right.right.as_ref().map_or(0u8, |r| r.height);
    if r_rh >= r_lh {
        // Right-right: single left rotation.
        rotate_left(left, hash, element, right)
    } else {
        // Right-left: rotate right child right, then rotate whole left.
        match &right.left {
            Some(rl) => {
                let new_right = rotate_right(
                    Arc::clone(rl),
                    right.hash,
                    Arc::clone(&right.element),
                    right.right.clone(),
                );
                rotate_left(left, hash, element, new_right)
            }
            None => Node::make(left, hash, element, Some(right)),
        }
    }
}

fn balanced<T: Tilable>(
    left: Option<Arc<Node<T>>>,
    hash: Hash,
    element: Arc<T>,
    right: Option<Arc<Node<T>>>,
) -> Arc<Node<T>> {
    let lh = left.as_ref().map_or(0i32, |l| l.height as i32);
    let rh = right.as_ref().map_or(0i32, |r| r.height as i32);
    let diff = lh - rh;
    if diff > 1 {
        match left {
            Some(l) => rebalance_left(l, hash, element, right),
            None => Node::make(None, hash, element, right),
        }
    } else if diff < -1 {
        match right {
            Some(r) => rebalance_right(left, hash, element, r),
            None => Node::make(left, hash, element, None),
        }
    } else {
        Node::make(left, hash, element, right)
    }
}

// --- recursive tree operations ---

/// Appends `element` as the rightmost leaf of `node`'s subtree.
fn append_node<T: Tilable>(
    node: &Option<Arc<Node<T>>>,
    hash: Hash,
    element: Arc<T>,
) -> Arc<Node<T>> {
    match node {
        None => Node::leaf(hash, element),
        Some(n) => {
            let new_right = append_node(&n.right, hash, element);
            balanced(
                n.left.clone(),
                n.hash,
                Arc::clone(&n.element),
                Some(new_right),
            )
        }
    }
}

/// Inserts `element` at `at_sample` relative to this subtree's origin.
/// `orig_sample` is the top-level sample (used verbatim in error values).
fn insert_node<T: Tilable>(
    node: &Option<Arc<Node<T>>>,
    at_sample: i64,
    orig_sample: i64,
    hash: Hash,
    element: Arc<T>,
) -> Result<Arc<Node<T>>, TreeError> {
    match node {
        None => {
            // Reached an empty subtree — only valid when appending (at_sample == 0).
            if at_sample == 0 {
                Ok(Node::leaf(hash, element))
            } else {
                Err(TreeError::SampleOutOfRange(orig_sample))
            }
        }
        Some(n) => {
            if at_sample < n.left_subtree_sum {
                let new_left = insert_node(&n.left, at_sample, orig_sample, hash, element)?;
                Ok(balanced(
                    Some(new_left),
                    n.hash,
                    Arc::clone(&n.element),
                    n.right.clone(),
                ))
            } else if at_sample == n.left_subtree_sum {
                // Boundary immediately before this node: append to left subtree.
                let new_left = append_node(&n.left, hash, element);
                Ok(balanced(
                    Some(new_left),
                    n.hash,
                    Arc::clone(&n.element),
                    n.right.clone(),
                ))
            } else {
                let in_offset = at_sample - n.left_subtree_sum;
                if in_offset < n.element.total_duration() {
                    Err(TreeError::SampleNotOnBoundary {
                        sample: orig_sample,
                        in_element_offset: in_offset,
                    })
                } else {
                    let right_at = at_sample - n.left_subtree_sum - n.element.total_duration();
                    let new_right = insert_node(&n.right, right_at, orig_sample, hash, element)?;
                    Ok(balanced(
                        n.left.clone(),
                        n.hash,
                        Arc::clone(&n.element),
                        Some(new_right),
                    ))
                }
            }
        }
    }
}

/// Replaces the element whose interval contains `sample` (relative to this subtree).
fn update_node<T: Tilable>(
    node: &Option<Arc<Node<T>>>,
    sample: i64,
    new_hash: Hash,
    new_element: Arc<T>,
) -> Result<Arc<Node<T>>, TreeError> {
    match node {
        None => Err(TreeError::SampleOutOfRange(sample)),
        Some(n) => {
            let offset = sample - n.left_subtree_sum;
            if offset < 0 {
                let new_left = update_node(&n.left, sample, new_hash, new_element)?;
                Ok(Node::make(
                    Some(new_left),
                    n.hash,
                    Arc::clone(&n.element),
                    n.right.clone(),
                ))
            } else if offset < n.element.total_duration() {
                Ok(Node::make(
                    n.left.clone(),
                    new_hash,
                    new_element,
                    n.right.clone(),
                ))
            } else {
                let right_sample = sample - n.left_subtree_sum - n.element.total_duration();
                let new_right = update_node(&n.right, right_sample, new_hash, new_element)?;
                Ok(Node::make(
                    n.left.clone(),
                    n.hash,
                    Arc::clone(&n.element),
                    Some(new_right),
                ))
            }
        }
    }
}

/// Removes and returns the leftmost node of `subtree`, plus the rebalanced remainder.
fn pop_leftmost<T: Tilable>(subtree: Arc<Node<T>>) -> (Arc<Node<T>>, Option<Arc<Node<T>>>) {
    match &subtree.left {
        None => (Arc::clone(&subtree), subtree.right.clone()),
        Some(left) => {
            let (leftmost, new_left) = pop_leftmost(Arc::clone(left));
            let remainder = balanced(
                new_left,
                subtree.hash,
                Arc::clone(&subtree.element),
                subtree.right.clone(),
            );
            (leftmost, Some(remainder))
        }
    }
}

/// Deletes the element whose interval contains `sample` (relative to this subtree).
fn delete_node<T: Tilable>(
    node: &Option<Arc<Node<T>>>,
    sample: i64,
) -> Result<Option<Arc<Node<T>>>, TreeError> {
    match node {
        None => Err(TreeError::SampleOutOfRange(sample)),
        Some(n) => {
            let offset = sample - n.left_subtree_sum;
            if offset < 0 {
                let new_left = delete_node(&n.left, sample)?;
                Ok(Some(balanced(
                    new_left,
                    n.hash,
                    Arc::clone(&n.element),
                    n.right.clone(),
                )))
            } else if offset < n.element.total_duration() {
                match (&n.left, &n.right) {
                    (None, None) => Ok(None),
                    (Some(l), None) => Ok(Some(Arc::clone(l))),
                    (None, Some(r)) => Ok(Some(Arc::clone(r))),
                    (Some(_), Some(right)) => {
                        let (succ, new_right) = pop_leftmost(Arc::clone(right));
                        Ok(Some(balanced(
                            n.left.clone(),
                            succ.hash,
                            Arc::clone(&succ.element),
                            new_right,
                        )))
                    }
                }
            } else {
                let right_sample = sample - n.left_subtree_sum - n.element.total_duration();
                let new_right = delete_node(&n.right, right_sample)?;
                Ok(Some(balanced(
                    n.left.clone(),
                    n.hash,
                    Arc::clone(&n.element),
                    new_right,
                )))
            }
        }
    }
}

/// Returns the hash of the rightmost node in `node`'s subtree.
fn rightmost_hash<T: Tilable>(node: &Option<Arc<Node<T>>>) -> Option<Hash> {
    let mut cur = node.as_deref()?;
    loop {
        match &cur.right {
            None => return Some(cur.hash),
            Some(r) => cur = r,
        }
    }
}

// --- public tree type ---

/// Duration-weighted, sequence-ordered AVL with structural sharing.
///
/// Clone is cheap (one `Arc` refcount bump). All mutation methods take `&self` and
/// return a new tree; the prior root remains valid for snapshot and undo.
/// See [data-model.md § Implicit timeline tree](data-model.md#implicit-timeline-tree).
pub struct ImplicitTimelineTree<T: Tilable> {
    root: Option<Arc<Node<T>>>,
}

impl<T: Tilable> Clone for ImplicitTimelineTree<T> {
    fn clone(&self) -> Self {
        ImplicitTimelineTree {
            root: self.root.clone(),
        }
    }
}

impl<T: Tilable> std::fmt::Debug for ImplicitTimelineTree<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ImplicitTimelineTree(..)")
    }
}

impl<T: Tilable> Default for ImplicitTimelineTree<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Tilable> PartialEq for ImplicitTimelineTree<T> {
    /// Sequence-equality: same `(hash, total_duration)` at each in-order position.
    ///
    /// Does NOT compare AVL shape — two trees built from the same ordered input via
    /// different construction paths are equal if their element sequences match.
    fn eq(&self, other: &Self) -> bool {
        let mut a = self.iter();
        let mut b = other.iter();
        loop {
            match (a.next(), b.next()) {
                (None, None) => return true,
                (Some(x), Some(y)) => {
                    if x.hash != y.hash || x.element.total_duration() != y.element.total_duration()
                    {
                        return false;
                    }
                }
                _ => return false,
            }
        }
    }
}

impl<T: Tilable> ImplicitTimelineTree<T> {
    /// Empty tree (no elements, total duration 0).
    pub fn new() -> Self {
        ImplicitTimelineTree { root: None }
    }

    /// O(n) build from an ordered `Vec` of `(hash, element)` pairs in timeline order.
    ///
    /// Input is consumed; the resulting AVL is balanced with `left_subtree_sum` and
    /// `height` computed bottom-up. The input is not re-sorted — callers must provide
    /// elements in timeline order.
    pub fn from_sorted_elements(elements: Vec<(Hash, Arc<T>)>) -> Self {
        fn build<T: Tilable>(slice: &[(Hash, Arc<T>)]) -> Option<Arc<Node<T>>> {
            if slice.is_empty() {
                return None;
            }
            let mid = slice.len() / 2;
            let left = build(&slice[..mid]);
            let right = build(&slice[mid + 1..]);
            Some(Node::make(
                left,
                slice[mid].0,
                Arc::clone(&slice[mid].1),
                right,
            ))
        }
        ImplicitTimelineTree {
            root: build(&elements),
        }
    }

    /// `true` when this tree has no elements.
    pub fn is_empty(&self) -> bool {
        self.root.is_none()
    }

    /// Number of elements in the tree.
    pub fn len(&self) -> usize {
        self.iter().count()
    }

    /// Sum of `total_duration()` over all elements (total track length in samples).
    pub fn total_duration(&self) -> i64 {
        self.root.as_ref().map_or(0, |n| n.total_subtree_sum)
    }

    /// In-order iterator yielding [`ElementRef`] items with running start samples.
    ///
    /// Walk the tree once; each yielded item carries the element's accumulated
    /// start sample so renderers never re-traverse.
    pub fn iter(&self) -> TreeIter<'_, T> {
        let mut stack = Vec::new();
        let mut cur = self.root.as_deref();
        while let Some(n) = cur {
            stack.push((n, 0i64));
            cur = n.left.as_deref();
        }
        TreeIter { stack }
    }

    /// Insert `element` at boundary sample `at_sample`.
    ///
    /// Valid boundaries:
    /// - `0` — insert as new head (also the only valid insert into an empty tree).
    /// - `total_duration()` — append as new tail.
    /// - The start sample of any existing element — insert before it.
    ///
    /// Elements are atomic; splitting one is a higher-level operation.
    ///
    /// # Errors
    /// - [`TreeError::SampleOutOfRange`] if `at_sample < 0` or
    ///   `at_sample > total_duration()`.
    /// - [`TreeError::SampleNotOnBoundary`] if `at_sample` falls strictly inside an
    ///   element's interior.
    pub fn insert_at(
        &self,
        at_sample: i64,
        element_hash: Hash,
        element: Arc<T>,
    ) -> Result<Self, TreeError> {
        if at_sample < 0 || at_sample > self.total_duration() {
            return Err(TreeError::SampleOutOfRange(at_sample));
        }
        let new_root = insert_node(&self.root, at_sample, at_sample, element_hash, element)?;
        Ok(ImplicitTimelineTree {
            root: Some(new_root),
        })
    }

    /// Replace the element whose interval contains `sample`.
    ///
    /// # Errors
    /// [`TreeError::SampleOutOfRange`] if `sample < 0` or `sample >= total_duration()`
    /// (including any call on an empty tree).
    pub fn update_at(
        &self,
        sample: i64,
        new_hash: Hash,
        new_element: Arc<T>,
    ) -> Result<Self, TreeError> {
        if sample < 0 || sample >= self.total_duration() {
            return Err(TreeError::SampleOutOfRange(sample));
        }
        let new_root = update_node(&self.root, sample, new_hash, new_element)?;
        Ok(ImplicitTimelineTree {
            root: Some(new_root),
        })
    }

    /// Delete the element whose interval contains `sample`.
    ///
    /// # Errors
    /// [`TreeError::SampleOutOfRange`] if `sample < 0` or `sample >= total_duration()`
    /// (including any call on an empty tree).
    pub fn delete_at(&self, sample: i64) -> Result<Self, TreeError> {
        if sample < 0 || sample >= self.total_duration() {
            return Err(TreeError::SampleOutOfRange(sample));
        }
        let new_root = delete_node(&self.root, sample)?;
        Ok(ImplicitTimelineTree { root: new_root })
    }

    /// In-order iterator positioned at the element covering `sample`.
    ///
    /// The first `next()` yields the element whose interval contains `sample` (or the
    /// element starting exactly at `sample`), carrying its true accumulated start sample;
    /// the walk then proceeds in timeline order. `sample <= 0` reproduces `iter()`;
    /// `sample >= total_duration()` yields an empty iterator. O(log n); `next()` reused.
    pub fn iter_from(&self, sample: i64) -> TreeIter<'_, T> {
        if self.root.is_none() || sample >= self.total_duration() {
            return TreeIter { stack: Vec::new() };
        }
        let sample = sample.max(0);

        let mut stack = Vec::new();
        let mut cur = self.root.as_deref();
        let mut base = 0i64;

        while let Some(node) = cur {
            let node_start = base + node.left_subtree_sum;
            if sample < node_start {
                // target is in the left subtree; push this node (yield after left)
                stack.push((node, base));
                cur = node.left.as_deref();
                // base unchanged: left subtree shares the same origin
            } else if sample >= node_start + node.element.total_duration() {
                // target is in the right subtree; skip this node
                base = node_start + node.element.total_duration();
                cur = node.right.as_deref();
            } else {
                // sample falls within this node; this is the target
                stack.push((node, base));
                break;
            }
        }

        TreeIter { stack }
    }

    /// Like [`iter_from`](Self::iter_from) but returns an [`OwnedTreeIter`] that holds
    /// `Arc<Node<T>>` clones rather than borrowed references, making it `'static + Send`.
    pub fn owned_iter_from(&self, sample: i64) -> OwnedTreeIter<T> {
        if self.root.is_none() || sample >= self.total_duration() {
            return OwnedTreeIter { stack: Vec::new() };
        }
        let sample = sample.max(0);

        let mut stack = Vec::new();
        let mut cur = self.root.clone();
        let mut base = 0i64;

        while let Some(node) = cur {
            let node_start = base + node.left_subtree_sum;
            if sample < node_start {
                let left = node.left.clone();
                stack.push((node, base));
                cur = left;
            } else if sample >= node_start + node.element.total_duration() {
                base = node_start + node.element.total_duration();
                cur = node.right.clone();
            } else {
                stack.push((node, base));
                break;
            }
        }

        OwnedTreeIter { stack }
    }

    /// Content hash of the last element in timeline order, or `None` if empty. O(log n).
    pub fn last_hash(&self) -> Option<Hash> {
        rightmost_hash(&self.root)
    }

    /// Locate the element covering project sample `t`.
    ///
    /// Returns `None` if `t < 0`, `t >= total_duration()`, or the tree is empty.
    /// O(log n). [`ElementHit::in_offset`] is non-negative and `< element.total_duration()`.
    pub fn element_at_sample(&self, t: i64) -> Option<ElementHit<T>> {
        if t < 0 {
            return None;
        }
        let mut node = self.root.as_deref()?;
        let mut last_right_ancestor: Option<Hash> = None;
        let mut remaining = t;
        loop {
            let offset = remaining - node.left_subtree_sum;
            if offset < 0 {
                node = node.left.as_deref()?;
            } else if offset < node.element.total_duration() {
                let predecessor = match &node.left {
                    Some(_) => rightmost_hash(&node.left),
                    None => last_right_ancestor,
                };
                return Some(ElementHit {
                    hash: node.hash,
                    element: Arc::clone(&node.element),
                    in_offset: offset,
                    predecessor,
                });
            } else {
                last_right_ancestor = Some(node.hash);
                remaining -= node.left_subtree_sum + node.element.total_duration();
                node = node.right.as_deref()?;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::project::label::{encode_label, Label, LabelKind};
    use crate::project::turn::{encode_turn, Turn};

    // --- deterministic seeded RNG (xorshift64) ---

    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Self {
            Rng(seed)
        }

        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn below(&mut self, n: usize) -> usize {
            (self.next() % n as u64) as usize
        }
    }

    // --- element factories ---

    fn turn_with(id: u64, turn_duration: i64, post_turn_silence: i64) -> (Hash, Arc<Turn>) {
        let t = Turn {
            id,
            speaker_id: None,
            turn_duration,
            post_turn_silence,
            words: vec![],
            splices: vec![],
        };
        let (h, _) = encode_turn(&t).unwrap();
        (h, Arc::new(t))
    }

    fn label_with(id: u64, post_label_silence: i64) -> (Hash, Arc<Label>) {
        let l = Label {
            id,
            text: String::new(),
            kind: LabelKind::Plain,
            post_label_silence,
        };
        let (h, _) = encode_label(&l).unwrap();
        (h, Arc::new(l))
    }

    // --- invariant checkers (access private Node fields via in-module access) ---

    fn check_height_balance<T: Tilable>(node: &Option<Arc<Node<T>>>) {
        if let Some(n) = node {
            let lh = n.left.as_ref().map_or(0u8, |l| l.height);
            let rh = n.right.as_ref().map_or(0u8, |r| r.height);
            assert!(
                (lh as i32 - rh as i32).abs() <= 1,
                "AVL height imbalance: lh={lh}, rh={rh}"
            );
            assert_eq!(n.height, 1 + lh.max(rh), "height augmentation wrong");
            check_height_balance(&n.left);
            check_height_balance(&n.right);
        }
    }

    fn subtree_sum<T: Tilable>(node: &Option<Arc<Node<T>>>) -> i64 {
        match node {
            None => 0,
            Some(n) => subtree_sum(&n.left) + n.element.total_duration() + subtree_sum(&n.right),
        }
    }

    fn check_augmentation<T: Tilable>(node: &Option<Arc<Node<T>>>) {
        if let Some(n) = node {
            let exp_lss = subtree_sum(&n.left);
            assert_eq!(
                n.left_subtree_sum, exp_lss,
                "left_subtree_sum wrong: got {}, expected {exp_lss}",
                n.left_subtree_sum
            );
            let lh = n.left.as_ref().map_or(0u8, |l| l.height);
            let rh = n.right.as_ref().map_or(0u8, |r| r.height);
            assert_eq!(n.height, 1 + lh.max(rh), "height augmentation wrong");
            check_augmentation(&n.left);
            check_augmentation(&n.right);
        }
    }

    fn hashes<T: Tilable>(tree: &ImplicitTimelineTree<T>) -> Vec<Hash> {
        tree.iter().map(|e| e.hash).collect()
    }

    fn start_samples<T: Tilable>(tree: &ImplicitTimelineTree<T>) -> Vec<i64> {
        tree.iter().map(|e| e.start_sample).collect()
    }

    // --- T1 ---

    #[test]
    fn new_is_empty() {
        let t = ImplicitTimelineTree::<Turn>::new();
        assert!(t.is_empty());
        assert_eq!(t.len(), 0);
        assert_eq!(t.total_duration(), 0);
        assert!(t.iter().next().is_none());

        let l = ImplicitTimelineTree::<Label>::new();
        assert!(l.is_empty());
        assert_eq!(l.len(), 0);
        assert_eq!(l.total_duration(), 0);
        assert!(l.iter().next().is_none());
    }

    // --- T2 ---

    #[test]
    fn bulk_build_empty() {
        let t = ImplicitTimelineTree::<Turn>::from_sorted_elements(vec![]);
        assert!(t.is_empty());
        assert_eq!(t.len(), 0);
    }

    // --- T3 ---

    #[test]
    fn bulk_build_single() {
        let (h, e) = turn_with(1, 100, 50);
        let tree = ImplicitTimelineTree::from_sorted_elements(vec![(h, e)]);
        assert_eq!(tree.len(), 1);
        assert_eq!(tree.total_duration(), 150);
        let mut it = tree.iter();
        let first = it.next().expect("one element");
        assert_eq!(first.start_sample, 0);
        assert!(it.next().is_none());

        let (lh, le) = label_with(1, 200);
        let ltree = ImplicitTimelineTree::from_sorted_elements(vec![(lh, le)]);
        assert_eq!(ltree.len(), 1);
        assert_eq!(ltree.total_duration(), 200);
        let lfirst = ltree.iter().next().expect("one label");
        assert_eq!(lfirst.start_sample, 0);
    }

    // --- T4 ---

    #[test]
    fn bulk_build_many_preserves_order() {
        let elems: Vec<(Hash, Arc<Turn>)> = (0u64..100)
            .map(|i| turn_with(i, (i as i64 + 1) * 100, 50))
            .collect();
        let exp_hashes: Vec<Hash> = elems.iter().map(|(h, _)| *h).collect();
        let total: i64 = elems.iter().map(|(_, e)| e.total_duration()).sum();
        let tree = ImplicitTimelineTree::from_sorted_elements(elems);
        assert_eq!(hashes(&tree), exp_hashes);
        assert_eq!(tree.total_duration(), total);

        let mut acc = 0i64;
        for e in tree.iter() {
            assert_eq!(e.start_sample, acc);
            acc += e.element.total_duration();
        }

        let lelems: Vec<(Hash, Arc<Label>)> = (0u64..100)
            .map(|i| label_with(i, (i as i64 + 1) * 77))
            .collect();
        let lexp: Vec<Hash> = lelems.iter().map(|(h, _)| *h).collect();
        let ltotal: i64 = lelems.iter().map(|(_, e)| e.total_duration()).sum();
        let ltree = ImplicitTimelineTree::from_sorted_elements(lelems);
        assert_eq!(hashes(&ltree), lexp);
        assert_eq!(ltree.total_duration(), ltotal);
    }

    // --- T5 ---

    #[test]
    fn bulk_build_is_height_balanced() {
        for &n in &[1usize, 2, 7, 100, 1000] {
            let elems: Vec<(Hash, Arc<Turn>)> =
                (0u64..n as u64).map(|i| turn_with(i, 100, 0)).collect();
            let tree = ImplicitTimelineTree::from_sorted_elements(elems);
            check_height_balance(&tree.root);

            let lelems: Vec<(Hash, Arc<Label>)> =
                (0u64..n as u64).map(|i| label_with(i, 100)).collect();
            let ltree = ImplicitTimelineTree::from_sorted_elements(lelems);
            check_height_balance(&ltree.root);
        }
    }

    // --- T6 ---

    #[test]
    fn augmentation_invariant_after_bulk_build() {
        for &n in &[1usize, 7, 100] {
            let elems: Vec<(Hash, Arc<Turn>)> = (0u64..n as u64)
                .map(|i| turn_with(i, (i as i64 + 1) * 50, 10))
                .collect();
            let tree = ImplicitTimelineTree::from_sorted_elements(elems);
            check_augmentation(&tree.root);
        }
    }

    // --- T7 ---

    #[test]
    fn incremental_append_via_insert_at_total_duration() {
        let mut tree = ImplicitTimelineTree::<Turn>::new();
        let mut exp_hashes: Vec<Hash> = Vec::new();
        let mut exp_total: i64 = 0;

        for i in 0u64..100 {
            let (h, e) = turn_with(i, 100, 50);
            exp_hashes.push(h);
            exp_total += e.total_duration();
            tree = tree.insert_at(tree.total_duration(), h, e).unwrap();
            assert_eq!(hashes(&tree), exp_hashes);
            assert_eq!(tree.total_duration(), exp_total);
            check_height_balance(&tree.root);
            check_augmentation(&tree.root);
        }
    }

    // --- T8 ---

    #[test]
    fn bulk_build_equals_incremental_append() {
        let elems: Vec<(Hash, Arc<Turn>)> = (0u64..100).map(|i| turn_with(i, 100, 50)).collect();
        let bulk = ImplicitTimelineTree::from_sorted_elements(elems.clone());

        let mut inc = ImplicitTimelineTree::<Turn>::new();
        for (h, e) in &elems {
            inc = inc
                .insert_at(inc.total_duration(), *h, Arc::clone(e))
                .unwrap();
        }
        assert_eq!(bulk, inc);

        let lelems: Vec<(Hash, Arc<Label>)> = (0u64..100).map(|i| label_with(i, 100)).collect();
        let lbulk = ImplicitTimelineTree::from_sorted_elements(lelems.clone());
        let mut linc = ImplicitTimelineTree::<Label>::new();
        for (h, e) in &lelems {
            linc = linc
                .insert_at(linc.total_duration(), *h, Arc::clone(e))
                .unwrap();
        }
        assert_eq!(lbulk, linc);
    }

    // --- T9 ---

    #[test]
    fn incremental_prepend_via_insert_at_zero() {
        let mut tree = ImplicitTimelineTree::<Turn>::new();
        let mut inserted: Vec<Hash> = Vec::new();

        for i in 0u64..50 {
            let (h, e) = turn_with(i, 100, 50);
            inserted.push(h);
            tree = tree.insert_at(0, h, e).unwrap();
            assert_eq!(tree.iter().next().unwrap().hash, h);
        }

        let mut exp = inserted.clone();
        exp.reverse();
        assert_eq!(hashes(&tree), exp);
    }

    // --- T10 ---

    #[test]
    fn incremental_insert_at_random_boundaries() {
        let mut rng = Rng::new(0xdeadbeef_cafebabe);

        let initial: Vec<(Hash, Arc<Turn>)> = (0u64..50).map(|i| turn_with(i, 100, 0)).collect();
        let mut mirror: Vec<(Hash, i64)> = initial
            .iter()
            .map(|(h, e)| (*h, e.total_duration()))
            .collect();
        let mut tree = ImplicitTimelineTree::from_sorted_elements(initial);

        for next_id in 50u64..250 {
            let idx = rng.below(mirror.len() + 1);
            let at: i64 = mirror[..idx].iter().map(|(_, d)| d).sum();
            let (h, e) = turn_with(next_id, 100, 0);
            let dur = e.total_duration();
            mirror.insert(idx, (h, dur));
            tree = tree.insert_at(at, h, e).unwrap();

            check_height_balance(&tree.root);
            check_augmentation(&tree.root);
            assert_eq!(
                hashes(&tree),
                mirror.iter().map(|(h, _)| *h).collect::<Vec<_>>()
            );
        }
    }

    // --- T11 ---

    #[test]
    fn random_deletes_preserve_avl_invariant() {
        let mut rng = Rng::new(0x1234_5678_9abc_def0);

        let initial: Vec<(Hash, Arc<Turn>)> = (0u64..200).map(|i| turn_with(i, 100, 0)).collect();
        let mut mirror: Vec<(Hash, i64)> = initial
            .iter()
            .map(|(h, e)| (*h, e.total_duration()))
            .collect();
        let mut tree = ImplicitTimelineTree::from_sorted_elements(initial);

        while !mirror.is_empty() {
            let idx = rng.below(mirror.len());
            let start: i64 = mirror[..idx].iter().map(|(_, d)| d).sum();
            let at = start + mirror[idx].1 / 2;
            mirror.remove(idx);
            tree = tree.delete_at(at).unwrap();

            check_height_balance(&tree.root);
            check_augmentation(&tree.root);
            assert_eq!(
                hashes(&tree),
                mirror.iter().map(|(h, _)| *h).collect::<Vec<_>>()
            );
        }
    }

    // --- T12 ---

    #[test]
    fn random_mixed_inserts_updates_deletes_preserve_avl_invariant() {
        let mut rng = Rng::new(0xfeedfacedeadbeef);

        let initial: Vec<(Hash, Arc<Turn>)> = (0u64..20).map(|i| turn_with(i, 100, 0)).collect();
        let mut mirror: Vec<(Hash, i64)> = initial
            .iter()
            .map(|(h, e)| (*h, e.total_duration()))
            .collect();
        let mut tree = ImplicitTimelineTree::from_sorted_elements(initial);
        let mut next_id = 20u64;

        for _ in 0..500 {
            let op = if mirror.is_empty() { 0 } else { rng.below(3) };
            match op {
                0 => {
                    let idx = rng.below(mirror.len() + 1);
                    let at: i64 = mirror[..idx].iter().map(|(_, d)| d).sum();
                    let (h, e) = turn_with(next_id, 100, 0);
                    next_id += 1;
                    mirror.insert(idx, (h, e.total_duration()));
                    tree = tree.insert_at(at, h, e).unwrap();
                }
                1 => {
                    let idx = rng.below(mirror.len());
                    let start: i64 = mirror[..idx].iter().map(|(_, d)| d).sum();
                    let at = start + mirror[idx].1 / 2;
                    let (h, e) = turn_with(next_id, mirror[idx].1, 0);
                    next_id += 1;
                    mirror[idx] = (h, e.total_duration());
                    tree = tree.update_at(at, h, e).unwrap();
                }
                _ => {
                    let idx = rng.below(mirror.len());
                    let start: i64 = mirror[..idx].iter().map(|(_, d)| d).sum();
                    let at = start + mirror[idx].1 / 2;
                    mirror.remove(idx);
                    tree = tree.delete_at(at).unwrap();
                }
            }

            check_height_balance(&tree.root);
            check_augmentation(&tree.root);
            assert_eq!(
                hashes(&tree),
                mirror.iter().map(|(h, _)| *h).collect::<Vec<_>>()
            );
        }
    }

    // --- T13 ---

    #[test]
    fn structural_sharing_preserved() {
        let elems: Vec<(Hash, Arc<Turn>)> = (0u64..50).map(|i| turn_with(i, 100, 0)).collect();
        let t0 = ImplicitTimelineTree::from_sorted_elements(elems);

        // Insert at sample 2600 (boundary after element 25, before element 26): the descent
        // goes right from root (lss=2500, elem_dur=100), so the LEFT subtree is untouched.
        let (h, e) = turn_with(999, 100, 0);
        let t1 = t0.insert_at(2600, h, e).unwrap();

        assert_eq!(t0.len(), 50);

        // Root's left child is on the untouched side and must share the same Arc.
        let l0 = t0.root.as_ref().unwrap().left.as_ref();
        let l1 = t1.root.as_ref().unwrap().left.as_ref();
        assert!(
            matches!((l0, l1), (Some(a), Some(b)) if Arc::ptr_eq(a, b)),
            "root's left subtree should be Arc-shared between t0 and t1"
        );
    }

    // --- T14 ---

    #[test]
    fn update_at_replaces_element_in_place() {
        let (ha, ea) = turn_with(1, 100, 0);
        let (hb, eb) = turn_with(2, 100, 0);
        let (hc, ec) = turn_with(3, 100, 0);
        let t0 = ImplicitTimelineTree::from_sorted_elements(vec![
            (ha, ea),
            (hb, Arc::clone(&eb)),
            (hc, ec),
        ]);

        let (hn, en) = turn_with(99, 100, 0);
        let t1 = t0.update_at(150, hn, en).unwrap(); // 150 is inside b

        assert_eq!(hashes(&t0), vec![ha, hb, hc]);
        assert_eq!(hashes(&t1), vec![ha, hn, hc]);
        assert_eq!(t1.total_duration(), t0.total_duration());

        let refs: Vec<_> = t1.iter().collect();
        assert_eq!(refs[1].hash, hn);
        assert_eq!(refs[1].start_sample, 100);
    }

    // --- T15 ---

    #[test]
    fn delete_at_removes_element() {
        let (ha, ea) = turn_with(1, 100, 0);
        let (hb, eb) = turn_with(2, 100, 0);
        let (hc, ec) = turn_with(3, 100, 0);
        let t0 = ImplicitTimelineTree::from_sorted_elements(vec![
            (ha, Arc::clone(&ea)),
            (hb, Arc::clone(&eb)),
            (hc, Arc::clone(&ec)),
        ]);

        assert_eq!(hashes(&t0.delete_at(50).unwrap()), vec![hb, hc]);
        assert_eq!(hashes(&t0.delete_at(150).unwrap()), vec![ha, hc]);
        assert_eq!(hashes(&t0.delete_at(250).unwrap()), vec![ha, hb]);
        assert_eq!(hashes(&t0), vec![ha, hb, hc]);
    }

    // --- T16 ---

    #[test]
    fn insert_at_zero_is_new_head() {
        let (ha, ea) = turn_with(1, 100, 0);
        let (hb, eb) = turn_with(2, 100, 0);
        let t0 = ImplicitTimelineTree::from_sorted_elements(vec![(ha, ea), (hb, eb)]);

        let (hx, ex) = turn_with(99, 50, 0);
        let t1 = t0.insert_at(0, hx, ex).unwrap();

        assert_eq!(hashes(&t1), vec![hx, ha, hb]);
        assert_eq!(start_samples(&t1), vec![0, 50, 150]);
    }

    // --- T17 ---

    #[test]
    fn insert_at_total_duration_appends() {
        let (ha, ea) = turn_with(1, 100, 0);
        let (hb, eb) = turn_with(2, 100, 0);
        let t0 = ImplicitTimelineTree::from_sorted_elements(vec![(ha, ea), (hb, eb)]);

        let (hx, ex) = turn_with(99, 50, 0);
        let t1 = t0.insert_at(t0.total_duration(), hx, ex).unwrap();

        assert_eq!(hashes(&t1), vec![ha, hb, hx]);
        assert_eq!(start_samples(&t1), vec![0, 100, 200]);
    }

    // --- T18 ---

    #[test]
    fn insert_at_interior_boundary() {
        let (ha, ea) = turn_with(1, 100, 0);
        let (hb, eb) = turn_with(2, 100, 0);
        let (hc, ec) = turn_with(3, 100, 0);
        let t0 = ImplicitTimelineTree::from_sorted_elements(vec![
            (ha, Arc::clone(&ea)),
            (hb, Arc::clone(&eb)),
            (hc, Arc::clone(&ec)),
        ]);

        let (hx1, ex1) = turn_with(91, 50, 0);
        let t1 = t0.insert_at(100, hx1, ex1).unwrap();
        assert_eq!(hashes(&t1), vec![ha, hx1, hb, hc]);

        let (hx2, ex2) = turn_with(92, 50, 0);
        let t2 = t0.insert_at(200, hx2, ex2).unwrap();
        assert_eq!(hashes(&t2), vec![ha, hb, hx2, hc]);
    }

    // --- T19 ---

    #[test]
    fn insert_at_into_empty_tree() {
        let (hx, ex) = turn_with(1, 100, 50);
        let t = ImplicitTimelineTree::<Turn>::new()
            .insert_at(0, hx, ex)
            .unwrap();
        assert_eq!(t.len(), 1);

        let (hy, ey) = turn_with(2, 100, 50);
        let err = ImplicitTimelineTree::<Turn>::new()
            .insert_at(1, hy, ey)
            .unwrap_err();
        assert!(matches!(err, TreeError::SampleOutOfRange(1)));
    }

    // --- T20 ---

    #[test]
    fn insert_at_interior_sample_errors() {
        let (ha, ea) = turn_with(1, 100, 0);
        let (hb, eb) = turn_with(2, 100, 0);
        let t0 = ImplicitTimelineTree::from_sorted_elements(vec![(ha, ea), (hb, eb)]);

        let (hx, ex) = turn_with(9, 50, 0);

        let err = t0.insert_at(50, hx, Arc::clone(&ex)).unwrap_err();
        assert!(
            matches!(
                err,
                TreeError::SampleNotOnBoundary {
                    sample: 50,
                    in_element_offset: 50
                }
            ),
            "got: {err:?}"
        );

        let err2 = t0.insert_at(150, hx, ex).unwrap_err();
        assert!(
            matches!(
                err2,
                TreeError::SampleNotOnBoundary {
                    sample: 150,
                    in_element_offset: 50
                }
            ),
            "got: {err2:?}"
        );
    }

    // --- T21 ---

    #[test]
    fn insert_at_negative_sample_errors() {
        let (ha, ea) = turn_with(1, 100, 0);
        let t0 = ImplicitTimelineTree::from_sorted_elements(vec![(ha, ea)]);
        let (hx, ex) = turn_with(9, 50, 0);
        assert!(matches!(
            t0.insert_at(-1, hx, ex).unwrap_err(),
            TreeError::SampleOutOfRange(-1)
        ));
    }

    // --- T22 ---

    #[test]
    fn insert_at_past_total_duration_errors() {
        let (ha, ea) = turn_with(1, 100, 0);
        let (hb, eb) = turn_with(2, 100, 0);
        let t0 = ImplicitTimelineTree::from_sorted_elements(vec![(ha, ea), (hb, eb)]);
        assert_eq!(t0.total_duration(), 200);

        let (hx, ex) = turn_with(9, 50, 0);
        assert!(matches!(
            t0.insert_at(201, hx, ex).unwrap_err(),
            TreeError::SampleOutOfRange(201)
        ));
    }

    // --- T23 ---

    #[test]
    fn update_at_empty_tree_errors() {
        let (hx, ex) = turn_with(1, 100, 0);
        assert!(matches!(
            ImplicitTimelineTree::<Turn>::new()
                .update_at(0, hx, ex)
                .unwrap_err(),
            TreeError::SampleOutOfRange(0)
        ));
    }

    // --- T24 ---

    #[test]
    fn update_at_past_end_errors() {
        let (ha, ea) = turn_with(1, 100, 0);
        let (hb, eb) = turn_with(2, 100, 0);
        let t0 = ImplicitTimelineTree::from_sorted_elements(vec![(ha, ea), (hb, eb)]);
        let (hx, ex) = turn_with(9, 50, 0);
        assert!(matches!(
            t0.update_at(200, hx, ex).unwrap_err(),
            TreeError::SampleOutOfRange(200)
        ));
    }

    // --- T25 ---

    #[test]
    fn update_at_negative_sample_errors() {
        let (ha, ea) = turn_with(1, 100, 0);
        let t0 = ImplicitTimelineTree::from_sorted_elements(vec![(ha, ea)]);
        let (hx, ex) = turn_with(9, 50, 0);
        assert!(matches!(
            t0.update_at(-1, hx, ex).unwrap_err(),
            TreeError::SampleOutOfRange(-1)
        ));
    }

    // --- T26 ---

    #[test]
    fn delete_at_empty_tree_errors() {
        assert!(matches!(
            ImplicitTimelineTree::<Turn>::new()
                .delete_at(0)
                .unwrap_err(),
            TreeError::SampleOutOfRange(0)
        ));
    }

    // --- T27 ---

    #[test]
    fn delete_at_past_end_errors() {
        let (ha, ea) = turn_with(1, 100, 0);
        let (hb, eb) = turn_with(2, 100, 0);
        let t0 = ImplicitTimelineTree::from_sorted_elements(vec![(ha, ea), (hb, eb)]);
        assert!(matches!(
            t0.delete_at(200).unwrap_err(),
            TreeError::SampleOutOfRange(200)
        ));
    }

    // --- T28 ---

    #[test]
    fn delete_until_empty_round_trips_to_new() {
        let (ha, ea) = turn_with(1, 100, 0);
        let (hb, eb) = turn_with(2, 100, 0);
        let (hc, ec) = turn_with(3, 100, 0);
        let t0 = ImplicitTimelineTree::from_sorted_elements(vec![(ha, ea), (hb, eb), (hc, ec)]);
        let t1 = t0.delete_at(0).unwrap();
        let t2 = t1.delete_at(0).unwrap();
        let t3 = t2.delete_at(0).unwrap();
        assert_eq!(t3, ImplicitTimelineTree::<Turn>::new());
    }

    // --- T29 ---

    #[test]
    fn element_at_sample_empty_tree() {
        let t = ImplicitTimelineTree::<Turn>::new();
        assert!(t.element_at_sample(0).is_none());
        assert!(t.element_at_sample(1).is_none());
        assert!(t.element_at_sample(i64::MAX).is_none());
        assert!(t.element_at_sample(-1).is_none());
    }

    // --- T30 ---

    #[test]
    fn element_at_sample_zero_is_first() {
        let (ha, ea) = turn_with(1, 100, 50);
        let (hb, eb) = turn_with(2, 80, 20);
        let tree = ImplicitTimelineTree::from_sorted_elements(vec![(ha, ea), (hb, eb)]);
        let hit = tree.element_at_sample(0).expect("should find element");
        assert_eq!(hit.hash, ha);
        assert_eq!(hit.in_offset, 0);
        assert!(hit.predecessor.is_none());
    }

    // --- T31 ---

    #[test]
    fn element_at_sample_last_sample() {
        let (ha, ea) = turn_with(1, 100, 0);
        let (hb, eb) = turn_with(2, 100, 0);
        let (hc, ec) = turn_with(3, 100, 0);
        let tree = ImplicitTimelineTree::from_sorted_elements(vec![
            (ha, ea),
            (hb, Arc::clone(&eb)),
            (hc, ec),
        ]);
        let hit = tree.element_at_sample(299).expect("last sample");
        assert_eq!(hit.hash, hc);
        assert_eq!(hit.in_offset, 99);
        assert_eq!(hit.predecessor, Some(hb));
    }

    // --- T32 ---

    #[test]
    fn element_at_sample_past_end_is_none() {
        let (ha, ea) = turn_with(1, 100, 0);
        let (hb, eb) = turn_with(2, 100, 0);
        let (hc, ec) = turn_with(3, 100, 0);
        let tree = ImplicitTimelineTree::from_sorted_elements(vec![(ha, ea), (hb, eb), (hc, ec)]);
        assert!(tree.element_at_sample(300).is_none());
        assert!(tree.element_at_sample(i64::MAX).is_none());
    }

    // --- T33 ---

    #[test]
    fn element_at_sample_lands_in_each_element() {
        let elems: Vec<(Hash, Arc<Turn>)> = (0u64..50)
            .map(|i| turn_with(i, (i as i64 + 1) * 100, 0))
            .collect();
        let exp_hashes: Vec<Hash> = elems.iter().map(|(h, _)| *h).collect();
        let tree = ImplicitTimelineTree::from_sorted_elements(elems.clone());

        let mut start = 0i64;
        for (idx, (_, e)) in elems.iter().enumerate() {
            let dur = e.total_duration();

            let hit = tree.element_at_sample(start).unwrap();
            assert_eq!(hit.hash, exp_hashes[idx]);
            assert_eq!(hit.in_offset, 0);
            assert_eq!(
                hit.predecessor,
                if idx == 0 {
                    None
                } else {
                    Some(exp_hashes[idx - 1])
                }
            );

            if dur > 1 {
                let hit2 = tree.element_at_sample(start + dur / 2).unwrap();
                assert_eq!(hit2.hash, exp_hashes[idx]);
                assert_eq!(hit2.in_offset, dur / 2);
            }

            start += dur;
        }
    }

    // --- T34 ---

    #[test]
    fn element_at_sample_turn_offset_distinguishes_speech_vs_silence() {
        let (h, e) = turn_with(1, 100, 50);
        let tree = ImplicitTimelineTree::from_sorted_elements(vec![(h, Arc::clone(&e))]);

        let hit_speech = tree.element_at_sample(50).unwrap();
        assert_eq!(hit_speech.hash, h);
        assert_eq!(hit_speech.in_offset, 50);
        assert!(hit_speech.in_offset < e.turn_duration);

        let hit_silence = tree.element_at_sample(120).unwrap();
        assert_eq!(hit_silence.hash, h);
        assert_eq!(hit_silence.in_offset, 120);
        assert!(hit_silence.in_offset >= e.turn_duration);
    }

    // --- T35 ---

    #[test]
    fn element_at_sample_label_offset_is_inter_label_gap() {
        let (h, e) = label_with(1, 100);
        let tree = ImplicitTimelineTree::from_sorted_elements(vec![(h, e)]);
        let hit = tree.element_at_sample(50).unwrap();
        assert_eq!(hit.hash, h);
        assert_eq!(hit.in_offset, 50);
    }

    // --- T36 ---

    #[test]
    fn element_at_sample_logn_with_balanced_tree() {
        let elems: Vec<(Hash, Arc<Turn>)> = (0u64..10_000).map(|i| turn_with(i, 100, 0)).collect();
        let exp_hashes: Vec<Hash> = elems.iter().map(|(h, _)| *h).collect();
        let tree = ImplicitTimelineTree::from_sorted_elements(elems);

        for (idx, sample) in [
            (0usize, 0i64),
            (1000, 100_000),
            (5000, 500_000),
            (7777, 777_700),
            (9999, 999_900),
        ] {
            let hit = tree.element_at_sample(sample).unwrap();
            assert_eq!(hit.hash, exp_hashes[idx]);
        }
    }

    // --- T37 ---

    #[test]
    fn iter_start_samples_are_running_prefix_sum() {
        let elems: Vec<(Hash, Arc<Turn>)> = (0u64..50)
            .map(|i| turn_with(i, (i as i64 + 1) * 37, 13))
            .collect();
        let tree = ImplicitTimelineTree::from_sorted_elements(elems);

        let mut acc = 0i64;
        for e in tree.iter() {
            assert_eq!(e.start_sample, acc);
            acc += e.element.total_duration();
        }

        let lelems: Vec<(Hash, Arc<Label>)> = (0u64..50)
            .map(|i| label_with(i, (i as i64 + 1) * 41))
            .collect();
        let ltree = ImplicitTimelineTree::from_sorted_elements(lelems);
        let mut lacc = 0i64;
        for e in ltree.iter() {
            assert_eq!(e.start_sample, lacc);
            lacc += e.element.total_duration();
        }
    }

    // --- T38 ---

    #[test]
    fn iter_after_random_edits_matches_mirror() {
        let elems: Vec<(Hash, Arc<Turn>)> = (0u64..5).map(|i| turn_with(i, 100, 0)).collect();
        let exp: Vec<Hash> = elems.iter().map(|(h, _)| *h).collect();
        let mut tree = ImplicitTimelineTree::from_sorted_elements(elems);

        // Insert hx (dur=50) at sample 300 (boundary between element 2 and element 3)
        // → sequence becomes [0, 1, 2, hx, 3, 4]
        let (hx, ex) = turn_with(99, 50, 0);
        tree = tree.insert_at(300, hx, ex).unwrap();

        let exp_hashes = vec![exp[0], exp[1], exp[2], hx, exp[3], exp[4]];
        let exp_starts: Vec<i64> = vec![0, 100, 200, 300, 350, 450];
        assert_eq!(hashes(&tree), exp_hashes);
        assert_eq!(start_samples(&tree), exp_starts);
    }

    // --- T39 ---

    #[test]
    fn eq_compares_in_order_sequence_not_shape() {
        let elems: Vec<(Hash, Arc<Turn>)> = (0u64..7).map(|i| turn_with(i, 100, 0)).collect();
        let ta = ImplicitTimelineTree::from_sorted_elements(elems.clone());

        let mut tb = ImplicitTimelineTree::<Turn>::new();
        for (h, e) in &elems {
            tb = tb
                .insert_at(tb.total_duration(), *h, Arc::clone(e))
                .unwrap();
        }
        assert_eq!(ta, tb);
    }

    // --- T40 ---

    #[test]
    fn eq_distinguishes_different_sequences() {
        let (ha, ea) = turn_with(1, 100, 0);
        let (hb, eb) = turn_with(2, 100, 0);
        let (hc, ec) = turn_with(3, 100, 0);

        let abc = ImplicitTimelineTree::from_sorted_elements(vec![
            (ha, Arc::clone(&ea)),
            (hb, Arc::clone(&eb)),
            (hc, Arc::clone(&ec)),
        ]);
        let acb = ImplicitTimelineTree::from_sorted_elements(vec![(ha, ea), (hc, ec), (hb, eb)]);
        assert_ne!(abc, acb);
    }

    // --- T41 ---

    #[test]
    fn eq_distinguishes_different_durations() {
        let (ha, ea) = turn_with(1, 100, 0);
        let (hb, eb) = turn_with(1, 200, 0); // same id, different duration → different hash
        let ta = ImplicitTimelineTree::from_sorted_elements(vec![(ha, ea)]);
        let tb = ImplicitTimelineTree::from_sorted_elements(vec![(hb, eb)]);
        assert_ne!(ta, tb);
    }

    // --- mutation-gap tests ---

    // Catches: `is_empty -> true` always (line 560).
    #[test]
    fn is_empty_false_for_non_empty_tree() {
        let (h, e) = turn_with(1, 100, 0);
        let tree = ImplicitTimelineTree::from_sorted_elements(vec![(h, Arc::clone(&e))]);
        assert!(!tree.is_empty());

        let inserted = ImplicitTimelineTree::<Turn>::new()
            .insert_at(0, h, e)
            .unwrap();
        assert!(!inserted.is_empty());
    }

    // Catches: `Clone::clone -> Default::default()` (line 486).
    #[test]
    fn clone_preserves_content() {
        let elems: Vec<(Hash, Arc<Turn>)> = (0u64..5).map(|i| turn_with(i, 100, 0)).collect();
        let tree = ImplicitTimelineTree::from_sorted_elements(elems);
        let cloned = tree.clone();
        assert_eq!(cloned, tree);
        assert_eq!(cloned.len(), 5);
    }

    // Catches: `update_at: sample < 0 → == 0` and `<= 0` (line 627 both variants).
    // update_at(0) on a non-empty tree must succeed, not return SampleOutOfRange.
    #[test]
    fn update_at_sample_zero_on_non_empty_succeeds() {
        let (ha, ea) = turn_with(1, 100, 0);
        let (hn, en) = turn_with(9, 100, 0);
        let tree = ImplicitTimelineTree::from_sorted_elements(vec![(ha, ea)]);
        let updated = tree.update_at(0, hn, en).unwrap();
        assert_eq!(hashes(&updated), vec![hn]);
    }

    // Catches: `update_node: offset < 0 → offset <= 0` (line 370).
    // update_at(sample) where sample == a non-root node's left_subtree_sum must find
    // that node, not recurse left and error.
    #[test]
    fn update_at_exact_start_of_non_root_element() {
        // Bulk-built [a, b] → root = b (lss=100). update_at(100) hits b with offset=0.
        // With the <= mutation, offset==0 recurses left → eventually SampleOutOfRange.
        let (ha, ea) = turn_with(1, 100, 0);
        let (hb, eb) = turn_with(2, 100, 0);
        let (hn, en) = turn_with(9, 100, 0);
        let tree = ImplicitTimelineTree::from_sorted_elements(vec![(ha, ea), (hb, eb)]);
        let updated = tree.update_at(100, hn, en).unwrap();
        assert_eq!(hashes(&updated), vec![ha, hn]); // b replaced, a unchanged
    }

    // Catches: `update_node: offset < elem_dur → offset <= elem_dur` (line 378).
    // update_at at the exact right boundary of an interior element must find the NEXT
    // element (by recursing right), not update the current one.
    #[test]
    fn update_at_right_boundary_of_element_finds_next() {
        // [a(100), b(100), c(100)] bulk-built → root=b (lss=100).
        // update_at(200): offset at b = 100 == b.total_duration → recurse right → hits c.
        // With <= mutation: 100 <= 100 → updates b instead of c.
        let (ha, ea) = turn_with(1, 100, 0);
        let (hb, eb) = turn_with(2, 100, 0);
        let (hc, ec) = turn_with(3, 100, 0);
        let (hn, en) = turn_with(9, 100, 0);
        let tree = ImplicitTimelineTree::from_sorted_elements(vec![
            (ha, ea),
            (hb, Arc::clone(&eb)),
            (hc, ec),
        ]);
        let updated = tree.update_at(200, hn, en).unwrap();
        assert_eq!(hashes(&updated), vec![ha, hb, hn]); // c replaced, b unchanged
    }

    // Catches: `delete_node: offset < elem_dur → offset <= elem_dur` (line 433).
    // delete_at at the exact right boundary of an interior element must delete the NEXT
    // element (by recursing right), not the current one.
    #[test]
    fn delete_at_right_boundary_of_element_deletes_next() {
        // [a(100), b(100), c(100)] bulk-built → root=b (lss=100).
        // delete_at(200): offset at b = 100 == b.total_duration → recurse right → deletes c.
        // With <= mutation: 100 <= 100 → deletes b instead of c (via in-order successor).
        let (ha, ea) = turn_with(1, 100, 0);
        let (hb, eb) = turn_with(2, 100, 0);
        let (hc, ec) = turn_with(3, 100, 0);
        let tree = ImplicitTimelineTree::from_sorted_elements(vec![
            (ha, Arc::clone(&ea)),
            (hb, Arc::clone(&eb)),
            (hc, Arc::clone(&ec)),
        ]);
        let deleted = tree.delete_at(200).unwrap();
        assert_eq!(hashes(&deleted), vec![ha, hb]); // c deleted, b unchanged
    }

    // --- I1 ---

    #[test]
    fn iter_from_seek_into_element_interior() {
        let elems: Vec<(Hash, Arc<Turn>)> = (0u64..5).map(|i| turn_with(i, 100, 0)).collect();
        let tree = ImplicitTimelineTree::from_sorted_elements(elems.clone());
        let exp: Vec<Hash> = elems.iter().map(|(h, _)| *h).collect();

        // sample 250 is inside element 2 which occupies [200, 300)
        let result: Vec<_> = tree.iter_from(250).collect();
        assert_eq!(result.len(), 3);
        assert_eq!(result[0].hash, exp[2]);
        assert_eq!(result[0].start_sample, 200);
        assert_eq!(result[1].hash, exp[3]);
        assert_eq!(result[1].start_sample, 300);
        assert_eq!(result[2].hash, exp[4]);
        assert_eq!(result[2].start_sample, 400);
    }

    // --- I2 ---

    #[test]
    fn iter_from_seek_to_exact_boundary() {
        let elems: Vec<(Hash, Arc<Turn>)> = (0u64..5).map(|i| turn_with(i, 100, 0)).collect();
        let tree = ImplicitTimelineTree::from_sorted_elements(elems.clone());
        let exp: Vec<Hash> = elems.iter().map(|(h, _)| *h).collect();

        // sample 300 = exact start of element 3
        let result: Vec<_> = tree.iter_from(300).collect();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].hash, exp[3]);
        assert_eq!(result[0].start_sample, 300);
        assert_eq!(result[1].hash, exp[4]);
        assert_eq!(result[1].start_sample, 400);
    }

    // --- I3 ---

    #[test]
    fn iter_from_zero_equals_iter() {
        let elems: Vec<(Hash, Arc<Turn>)> = (0u64..20)
            .map(|i| turn_with(i, 100 + i as i64, 10))
            .collect();
        let tree = ImplicitTimelineTree::from_sorted_elements(elems);

        let iter_seq: Vec<_> = tree.iter().map(|e| (e.hash, e.start_sample)).collect();
        let from_seq: Vec<_> = tree
            .iter_from(0)
            .map(|e| (e.hash, e.start_sample))
            .collect();
        assert_eq!(iter_seq, from_seq);
    }

    // --- I4 ---

    #[test]
    fn iter_from_edge_cases() {
        let (ha, ea) = turn_with(1, 100, 0);
        let (hb, eb) = turn_with(2, 100, 0);
        let tree = ImplicitTimelineTree::from_sorted_elements(vec![(ha, ea), (hb, eb)]);

        let iter_hashes: Vec<Hash> = tree.iter().map(|e| e.hash).collect();

        // sample < 0 reproduces iter()
        let neg: Vec<Hash> = tree.iter_from(-1).map(|e| e.hash).collect();
        assert_eq!(neg, iter_hashes);

        // sample == total_duration() yields empty (half-open [start, end))
        assert!(tree.iter_from(200).next().is_none());

        // sample > total_duration() yields empty
        assert!(tree.iter_from(500).next().is_none());

        // empty tree
        let empty: ImplicitTimelineTree<Turn> = ImplicitTimelineTree::new();
        assert!(empty.iter_from(0).next().is_none());
        assert!(empty.iter_from(50).next().is_none());
    }

    // --- I5 ---

    #[test]
    fn iter_from_random_seeks_match_linear_scan() {
        let mut rng = Rng::new(0x9876_5432_abcd_ef01);
        let elems: Vec<(Hash, Arc<Turn>)> = (0u64..30)
            .map(|i| {
                let dur = (rng.next() % 200 + 50) as i64;
                turn_with(i, dur, 10)
            })
            .collect();
        let tree = ImplicitTimelineTree::from_sorted_elements(elems);
        let all: Vec<_> = tree.iter().collect();

        let mut rng2 = Rng::new(0xfedc_ba98_7654_3210);
        for _ in 0..30 {
            let s = (rng2.next() % tree.total_duration() as u64) as i64;

            let from_seq: Vec<_> = tree
                .iter_from(s)
                .map(|e| (e.hash, e.start_sample))
                .collect();

            // Reference: keep elements whose interval end is strictly after s
            let linear_seq: Vec<_> = all
                .iter()
                .filter(|e| e.start_sample + e.element.total_duration() > s)
                .map(|e| (e.hash, e.start_sample))
                .collect();

            assert_eq!(from_seq, linear_seq, "seek to sample {s} failed");
        }
    }

    // LH1 — last_hash is None on an empty tree.
    #[test]
    fn last_hash_none_on_empty() {
        let tree: ImplicitTimelineTree<Turn> = ImplicitTimelineTree::new();
        assert!(tree.last_hash().is_none());
    }

    // LH2 — last_hash equals the sole element's hash on a singleton.
    #[test]
    fn last_hash_singleton() {
        let (ha, ea) = turn_with(1, 100, 0);
        let tree = ImplicitTimelineTree::from_sorted_elements(vec![(ha, ea)]);
        assert_eq!(tree.last_hash(), Some(ha));
    }

    // LH3 — last_hash returns the rightmost element on a multi-element tree.
    #[test]
    fn last_hash_rightmost_on_multi() {
        let (ha, ea) = turn_with(1, 100, 0);
        let (hb, eb) = turn_with(2, 100, 0);
        let (hc, ec) = turn_with(3, 100, 0);
        let tree = ImplicitTimelineTree::from_sorted_elements(vec![(ha, ea), (hb, eb), (hc, ec)]);
        assert_eq!(tree.last_hash(), Some(hc));
    }

    // LH4 — last_hash equals iter().last().map(|e| e.hash) across a randomized tree.
    //        Pins the O(log n) path to the O(n) semantic reference.
    #[test]
    fn last_hash_matches_iter_last_randomized() {
        let mut rng = Rng::new(0xABCD_1234_5678_EF00);
        let mut tree: ImplicitTimelineTree<Turn> = ImplicitTimelineTree::new();
        for id in 1u64..=32 {
            let dur = (rng.next() % 200 + 50) as i64;
            let (h, e) = turn_with(id, dur, 0);
            tree = tree.insert_at(tree.total_duration(), h, e).unwrap();
        }
        let expected = tree.iter().last().map(|e| e.hash);
        assert_eq!(tree.last_hash(), expected);
    }

    // MT1 — MergedTurns interleaves two trees in global start order.
    //       Trees tile gaplessly from local 0, so both contribute a turn at 0 (ties break to the
    //       lower-indexed tree); the rest interleave strictly by start_sample. end == start +
    //       turn_duration (the speech span), and a single tree degenerates to tree.iter().
    #[test]
    fn mt1_merged_turns_global_order() {
        // Tree A: id=1 [0,200) (dur 100, sil 100), id=3 [200,300) (dur 100). starts 0, 200.
        let mut a: ImplicitTimelineTree<Turn> = ImplicitTimelineTree::new();
        for (id, dur, sil) in [(1u64, 100i64, 100i64), (3, 100, 0)] {
            let (h, e) = turn_with(id, dur, sil);
            a = a.insert_at(a.total_duration(), h, e).unwrap();
        }
        // Tree B: id=2 [0,100), id=4 [100,300) (dur 100, sil 100), id=6 [300,400). starts 0,100,300.
        let mut b: ImplicitTimelineTree<Turn> = ImplicitTimelineTree::new();
        for (id, dur, sil) in [(2u64, 100i64, 0i64), (4, 100, 100), (6, 100, 0)] {
            let (h, e) = turn_with(id, dur, sil);
            b = b.insert_at(b.total_duration(), h, e).unwrap();
        }

        let merged: Vec<(i64, i64, u64)> = MergedTurns::new(&[(0, &a), (0, &b)])
            .map(|(start, end, turn)| (start, end, turn.id))
            .collect();

        // Global start order, ties (start 0) on the lower-indexed tree A first.
        assert_eq!(
            merged,
            vec![
                (0, 100, 1),
                (0, 100, 2),
                (100, 200, 4),
                (200, 300, 3),
                (300, 400, 6),
            ],
            "MT1: interleaved by start_sample; end == start + turn_duration"
        );

        // Single-tree merge degenerates to tree.iter().
        let single: Vec<u64> = MergedTurns::new(&[(0, &a)]).map(|(_, _, t)| t.id).collect();
        assert_eq!(single, vec![1, 3], "MT1: single tree == in-order iter");
    }

    // MT2 — each track's project_start_sample shifts its turns onto the project timeline, so
    //        tracks beginning at different offsets interleave in true global order. With local
    //        starts alone (offset ignored) the order would be wrong: B's id=2 starts at local 0
    //        but project 250, so it must fall after A's id=1 (project 0) and id=3 (project 200).
    #[test]
    fn mt2_merged_turns_honour_project_offsets() {
        // Tree A @ offset 0: id=1 [0,200) (dur 100, sil 100), id=3 [200,300) (dur 100).
        //   → project starts 0, 200.
        let mut a: ImplicitTimelineTree<Turn> = ImplicitTimelineTree::new();
        for (id, dur, sil) in [(1u64, 100i64, 100i64), (3, 100, 0)] {
            let (h, e) = turn_with(id, dur, sil);
            a = a.insert_at(a.total_duration(), h, e).unwrap();
        }
        // Tree B @ offset 250: id=2 [0,100), id=4 [100,200) (dur 100).
        //   → project starts 250, 350.
        let mut b: ImplicitTimelineTree<Turn> = ImplicitTimelineTree::new();
        for (id, dur, sil) in [(2u64, 100i64, 0i64), (4, 100, 0)] {
            let (h, e) = turn_with(id, dur, sil);
            b = b.insert_at(b.total_duration(), h, e).unwrap();
        }

        let merged: Vec<(i64, i64, u64)> = MergedTurns::new(&[(0, &a), (250, &b)])
            .map(|(start, end, turn)| (start, end, turn.id))
            .collect();

        // Ordered by project position (local start + offset), not local start alone.
        assert_eq!(
            merged,
            vec![(0, 100, 1), (200, 300, 3), (250, 350, 2), (350, 450, 4),],
            "MT2: turns ordered by project position, offsets applied to start and end"
        );
    }

    // --- owned_iter_from (the `'static` mirror of iter_from) ------------------

    // OwnedTreeIter / owned_iter_from carry their own start-sample accumulation and seek
    // logic, distinct from the (well-tested) borrowed TreeIter. Pin the exact start samples
    // an owned walk yields across seeks: interior, exact boundary, edges, past-end.
    #[test]
    fn owned_iter_from_exact_start_samples() {
        let elems: Vec<(Hash, Arc<Turn>)> = (0u64..5).map(|i| turn_with(i, 100, 0)).collect();
        let tree = ImplicitTimelineTree::from_sorted_elements(elems.clone());
        let exp: Vec<Hash> = elems.iter().map(|(h, _)| *h).collect();
        let collect = |s: i64| -> Vec<(i64, Hash)> {
            tree.owned_iter_from(s)
                .map(|e| (e.start_sample, e.hash))
                .collect()
        };

        // Full walk (seek 0 and negative both reproduce iter()).
        let full = vec![
            (0, exp[0]),
            (100, exp[1]),
            (200, exp[2]),
            (300, exp[3]),
            (400, exp[4]),
        ];
        assert_eq!(collect(0), full, "owned_iter_from(0)");
        assert_eq!(collect(-1), full, "negative seek reproduces full walk");
        // Interior of element 2 ([200,300)).
        assert_eq!(
            collect(250),
            vec![(200, exp[2]), (300, exp[3]), (400, exp[4])],
            "owned_iter_from(250) starts at the covering element with its true start"
        );
        // Exact boundary = start of element 3.
        assert_eq!(
            collect(300),
            vec![(300, exp[3]), (400, exp[4])],
            "owned_iter_from(300) starts exactly at element 3"
        );
        // total_duration() and past-end yield empty (half-open).
        assert!(collect(500).is_empty(), "seek == total is empty");
        assert!(collect(900).is_empty(), "seek past end is empty");
    }

    // owned_iter_from must agree with the borrowed iter_from element-for-element across random
    // seeks and durations — any owned-only divergence (start accumulation, seek descent) fails.
    #[test]
    fn owned_iter_from_matches_iter_from_randomized() {
        let mut rng = Rng::new(0x1234_5678_9abc_def0);
        let elems: Vec<(Hash, Arc<Turn>)> = (0u64..30)
            .map(|i| turn_with(i, (rng.next() % 200 + 50) as i64, 10))
            .collect();
        let tree = ImplicitTimelineTree::from_sorted_elements(elems);
        let total = tree.total_duration();

        let mut seeks = vec![-5, 0, 1, total - 1, total, total + 100];
        let mut rng2 = Rng::new(0x0fed_cba9_8765_4321);
        for _ in 0..20 {
            seeks.push((rng2.next() % total as u64) as i64);
        }
        for s in seeks {
            let borrowed: Vec<(i64, Hash)> = tree
                .iter_from(s)
                .map(|e| (e.start_sample, e.hash))
                .collect();
            let owned: Vec<(i64, Hash)> = tree
                .owned_iter_from(s)
                .map(|e| (e.start_sample, e.hash))
                .collect();
            assert_eq!(owned, borrowed, "owned_iter_from({s}) must match iter_from");
        }
    }

    // --- error / debug formatting --------------------------------------------

    #[test]
    fn tree_error_display_messages() {
        assert_eq!(
            format!("{}", TreeError::SampleOutOfRange(42)),
            "sample 42 is out of range"
        );
        let s = format!(
            "{}",
            TreeError::SampleNotOnBoundary {
                sample: 7,
                in_element_offset: 3,
            }
        );
        assert!(
            s.contains("sample 7 is not on an element boundary"),
            "got: {s}"
        );
        assert!(s.contains("in-element offset: 3"), "got: {s}");
    }

    #[test]
    fn tree_debug_is_opaque() {
        let tree: ImplicitTimelineTree<Turn> = ImplicitTimelineTree::new();
        assert_eq!(format!("{tree:?}"), "ImplicitTimelineTree(..)");
    }
}
