# Phase 1 · M1 · Step 6 — Implicit timeline tree (`project/tree.rs`) (action plan)

Per-step action plan for Step 6 of the M1 milestone from
[phase1-m1.md](phase1-m1.md). The authoritative spec is
[data-model.md § Implicit timeline tree](../design/data-model.md#implicit-timeline-tree),
[§ Temporal query](../design/data-model.md#temporal-query), and
[§ Tilable trait](../design/data-model.md#tilable-trait). This step lands the
**generic, immutable, duration-weighted AVL** that Phase 1 uses to model a
single track's timeline. Speech tracks instantiate it as
`ImplicitTimelineTree<Turn>`; track 0 as `ImplicitTimelineTree<Label>`.

**Definition of done:** `core/src/project/tree.rs` exposes a public
`ImplicitTimelineTree<T: Tilable>` over a private `Node<T>`. The tree
supports O(n) bulk-build, **sample-keyed** `insert_at` / `update_at` /
`delete_at` with AVL rebalancing and structural sharing, in-order
iteration that yields running start samples, and the temporal query
`element_at_sample` (which also surfaces the predecessor hash needed
for delta-location computation at edit time). The augmentation
(`left_subtree_sum`, `height`) is **derived, never serialized**. Full
unit coverage instantiated against both `Turn` and `Label`. `cargo
test -p core project::tree::`, `cargo clippy -p core -- -D warnings`,
and `cargo fmt --check` are all green.

## Context

[Step 3](phase1-m1-03.md) shipped [`Hash`](../src-tauri/core/src/project/hash.rs)
(16-byte BLAKE3-128) — the value the tree carries alongside each
element. [Step 4](phase1-m1-04.md) shipped the
[`Tilable`](../src-tauri/core/src/project/tilable.rs) trait with
`fn total_duration(&self) -> i64`, plus `impl Tilable for Turn` and
`impl Tilable for Label`. The trait is the **only** assumption the
tree makes about its element type. [Step 5](phase1-m1-05.md) shipped
the blob-store I/O primitives — orthogonal to this step (the tree
never calls `store::put` / `get`; Step 8's snapshot/replay does).

Step 6 lays down the in-memory data structure that:

1. Replay (Step 8) bulk-builds from an ordered hash sequence after
   walking the snapshot's adjacency list. **Replay never calls the
   mutation primitives** — the adjacency list is the working data
   structure that absorbs delta application, and `from_sorted_elements`
   is the single tree-construction sweep at the end.
2. Edits (M4/M5 commands) mutate the tree in-place at the edit
   sample, producing a new tree via path-copy, and separately record
   a `Vec<Delta>` (hash-keyed `Location::After(Hash) | Start`) on the
   journal. Tree mutation and delta production are sibling outputs
   of a single edit pass; neither drives the other.
3. The UI queries for "what element is at sample T?" (O(log n)) and
   renders the timeline by iterating the tree once, accumulating
   start samples as it goes (`iter()` yields each element's running
   start sample so the renderer never re-traverses).

No code outside `core/src/project/` consumes the tree in this step;
Step 8 (snapshot) and Step 11 (engine) are the first non-test
callers.

## Decisions locked in this step

- **Public type is `ImplicitTimelineTree<T: Tilable>`; `Node<T>` is
  private to the module.** Nodes are an implementation detail (AVL
  augmentation, structural sharing); tests reach into them via the
  same `#[cfg(test)] mod tests` that lives inside `tree.rs`, so no
  `pub(crate)` escape is needed.

- **`Node<T>` stores the on-disk `Hash` next to the `Arc<T>`.** This
  matches phase1-m1-04.md's downstream note ("the tree carries
  `Arc<T>` plus the on-disk `Hash` … the V_N hash seen on disk (load
  path) or `store_{turn,label}(&t).0` (edit path), never recomputed
  from the upgraded in-memory element"). Storing the hash lets
  `iter()` surface it for snapshot flattening and renderer cache
  population without re-serializing every element.

  The illustrative `struct Node<T>` example in
  [data-model.md § Implicit timeline tree](../design/data-model.md#implicit-timeline-tree)
  omits the `hash` field; **this step updates data-model.md to
  include it** (see [Documentation touches](#documentation-touches)).

- **Nodes are immutable; edits path-copy via `Arc<Node<T>>`.** No
  parent pointers (a shared subtree may sit under many roots). All
  mutation methods take `&self` and return a fresh
  `ImplicitTimelineTree<T>` whose root is a new `Arc` along the
  copied path; untouched subtrees are shared by `Arc::clone`. This
  is the substrate for cheap snapshot (Step 11) and pointer-restore
  undo (Step 10).

- **Augmentation is `(left_subtree_sum: i64, height: u8)`.**
  `left_subtree_sum` is Σ `total_duration()` over the left subtree
  (used by [§ Temporal query](../design/data-model.md#temporal-query));
  `height` is the AVL balance factor. Both recomputed at every node
  on the copied path; **neither is serialized** anywhere — the tree
  itself never goes to disk, and the snapshot blob (Step 8) only
  flattens the ordered hash sequence. The implementer MAY add a
  redundant `total_subtree_sum` field for O(1) sibling-sum lookup;
  see [implementation notes](#implementation-notes) below.

- **The tree's identity for mutation is the project-rate sample
  position of the affected element.** Not its hash (which would
  force an O(n) scan per mutation — the tree is ordered by timeline
  position, not by hash), and not a pointer or persistent ID (same
  scan problem, plus parent pointers would break structural
  sharing). Sample positions reach the affected element in O(log n)
  via the existing temporal-query descent and align with the
  edit-time data flow: the UI passes an edit sample to a command,
  the command queries the tree at that sample, and applies its
  mutation at the same sample.

  This is a deliberate departure from the **delta** language, where
  every operation is keyed by `Location { Start | After(Hash) }` —
  hashes are the right identity at the **journal** layer because
  the persistent store is content-addressed and the journal must
  survive renames, reorders, and replay. The tree, by contrast, is
  an in-memory structure whose only "key" is implicit position.
  Forcing the journal's identity onto the tree would re-introduce
  the O(n) hash scan with no benefit — delta application happens
  on the working **adjacency list** during replay (Step 8), not on
  the tree.

- **Mutation primitives address the affected element directly, not
  its predecessor.** Concretely:

  ```rust
  /// Insert `element` at the given boundary sample. `at_sample`
  /// must be 0, an element boundary, or `total_duration()`
  /// (which appends).
  pub fn insert_at(&self, at_sample: i64,
                   element_hash: Hash, element: Arc<T>)
      -> Result<Self, TreeError>;

  /// Replace the element whose interval contains `sample`.
  pub fn update_at(&self, sample: i64,
                   new_hash: Hash, new_element: Arc<T>)
      -> Result<Self, TreeError>;

  /// Delete the element whose interval contains `sample`.
  pub fn delete_at(&self, sample: i64) -> Result<Self, TreeError>;
  ```

  - `insert_at(0, …)` is the only valid insert into an empty tree
    (it places the new element as the sole element).
  - `insert_at(total_duration(), …)` appends as the new tail.
  - `insert_at(b, …)` where `b` is an interior element boundary
    (the start sample of element *i*) inserts the new element as
    element *i* (the old element *i* and all subsequent elements
    shift right by `element.total_duration()` samples).
  - `insert_at(s, …)` where `s` falls inside an element's interior
    returns `TreeError::SampleNotOnBoundary { sample: s,
    in_element_offset }` — elements are atomic; splitting one is a
    higher-level operation the caller composes from
    `update_at` + `insert_at` after computing the two halves.
  - `update_at(s, …)` / `delete_at(s)` accept any `s` within the
    addressable range `[0, total_duration())` — every such `s`
    unambiguously names exactly one element.

  This shape is more direct than the predecessor-keyed alternative
  inherited from the delta language (no "compute the
  predecessor first" step at the call site), and it makes the
  boundary discipline visible: an insert that wants to land
  mid-element is a logical error the caller must split into two
  ops.

- **Errors are `SampleOutOfRange(i64)` and `SampleNotOnBoundary
  { sample: i64, in_element_offset: i64 }`.** No
  `PredecessorNotFound`, no `EmptyTree` — both fold into
  `SampleOutOfRange` (an empty tree has `total_duration() == 0`, so
  every non-zero `sample` is out of range, and `insert_at(0)` on an
  empty tree is the valid sole-element case).

  ```rust
  pub enum TreeError {
      /// `sample` is negative or outside the operation's valid range.
      /// For insert_at: valid range is [0, total_duration()].
      /// For update_at / delete_at: valid range is [0, total_duration()).
      SampleOutOfRange(i64),
      /// insert_at requires `sample` to be 0, an element boundary,
      /// or total_duration(). The provided `sample` fell inside an
      /// element's interior; `in_element_offset` is the distance from
      /// that element's start sample (always > 0 and < the element's
      /// total_duration).
      SampleNotOnBoundary {
          /// The sample passed to insert_at.
          sample: i64,
          /// Offset within the element that contains `sample`.
          in_element_offset: i64,
      },
  }
  ```

- **`iter()` yields `ElementRef<'_, T>` carrying the running start
  sample.** This is the renderer's natural primitive: walk the tree
  once, building a `Vec<(start_sample, hash, &Arc<T>)>` for layout.
  No per-element start-sample lookup is needed at runtime. Drops the
  `start_sample_of(hash)` API that earlier drafts proposed (it would
  have been O(n) for the same reasons predecessor-by-hash would have
  been — the tree is ordered by position, not by hash — and there is
  no in-engine use case that has a hash without already having a
  sample).

  ```rust
  pub struct ElementRef<'a, T> {
      /// Project-rate sample at which this element begins.
      pub start_sample: i64,
      /// Content hash of the element.
      pub hash: Hash,
      /// Shared pointer to the element payload.
      pub element: &'a Arc<T>,
  }
  ```

- **No `predecessor_of(hash)` / `successor_of(hash)`.** Same
  rationale as `start_sample_of`: the tree is position-ordered, not
  hash-keyed. The one legitimate edit-time need — "what hash should
  the delta name as `Location::After(…)`?" — is satisfied two
  different ways depending on the operation:
  - **Update / Delete** at sample `s`: the delta's predecessor is
    the element BEFORE the affected element; this is
    `tree.element_at_sample(s)?.predecessor` — surfaced inline on
    the same query that drove the mutation, O(log n).
  - **Insert** at sample `s`: the delta's predecessor is the
    element ENDING at sample `s`; this is
    `tree.element_at_sample(s - 1)?.hash` (or `Location::Start`
    when `s == 0`). One extra O(log n) query.

  Neither path needs a hash-keyed traversal of the tree.

- **`ElementHit` is the return type of `element_at_sample`.**
  Carries the element's hash, the shared `Arc`, the in-element
  offset (raw `i64`; per-kind interpretation lives outside the
  tree — see [phase1-m1.md § Step 6](phase1-m1.md#step-6--implicit-timeline-tree-projecttreers)),
  and the predecessor hash (`None` if the hit is the first element).

  ```rust
  pub struct ElementHit<T> {
      pub hash:        Hash,
      pub element:     Arc<T>,
      pub in_offset:   i64,
      pub predecessor: Option<Hash>,
  }
  ```

- **Mutation primitives take `Arc<T>` by move, not by `&T` clone.**
  Callers already own an `Arc<T>` (either freshly loaded by
  `load_turn` / `load_label` or constructed and shared). Moving the
  `Arc` is one refcount increment; cloning the inner `T` would
  force every caller to clone non-trivial element payloads (a
  `Turn` with many words) for no reason.

- **`from_sorted_elements` is the O(n) bulk-build path.** Recursive
  middle-element-as-root construction over `Vec<(Hash, Arc<T>)>`
  produces a balanced AVL with computed augmentation in O(n) — used
  by replay (Step 8). The input is already in timeline order; the
  function does not re-sort.

- **`PartialEq for ImplicitTimelineTree<T>` compares in-order
  element sequences, not tree shapes.** Two trees built from the
  same ordered input may have different AVL shapes if one was
  bulk-built and the other was incrementally inserted (the AVL
  invariant constrains height, not exact rotation choices). For the
  replay-equivalence test in Step 8 ("incremental build == replay")
  and for tests in this step, the meaningful equivalence is
  sequence-equality. Shape equality is exposed separately as a
  test-only helper if a specific test ever needs it.

  The `PartialEq` comparison covers `(hash, total_duration)` pairs
  at each in-order position; the `Arc<T>` payloads are not compared
  pointer-wise (a load-via-replay path produces fresh `Arc`s with
  the same hash, which is the canonical identity).

- **No `Eq` on the tree.** `Turn`'s `f64` word-second fields
  prevent `Eq` on `Turn`, and `PartialEq` propagates. (Label has
  `Eq` but the generic bound on tree must accommodate both.)

- **No `Drop` impl.** `Arc` reference counting handles cleanup; no
  arena, no free list.

## Module surface

### New: `core/src/project/tree.rs`

```rust
//! Implicit timeline tree: immutable, duration-weighted AVL with structural sharing.
//!
//! Generic over the element type via [`Tilable`]; instantiated as
//! `ImplicitTimelineTree<Turn>` on speech tracks and `ImplicitTimelineTree<Label>`
//! on track 0. Edits path-copy to the root via `Arc<Node<T>>`, leaving prior roots
//! intact for snapshot and undo. See
//! [data-model.md § Implicit timeline tree](../design/data-model.md#implicit-timeline-tree).

use std::sync::Arc;

use super::hash::Hash;
use super::tilable::Tilable;

/// Errors returned by tree mutations.
#[derive(Debug)]
pub enum TreeError {
    /// `sample` is negative or outside the operation's valid range.
    SampleOutOfRange(i64),
    /// `insert_at` requires a boundary sample; the provided sample
    /// fell inside an element's interior.
    SampleNotOnBoundary {
        /// The sample passed to `insert_at`.
        sample: i64,
        /// Offset from the containing element's start sample;
        /// `0 < in_element_offset < element.total_duration()`.
        in_element_offset: i64,
    },
}

impl std::fmt::Display for TreeError { /* two arms */ }
impl std::error::Error for TreeError {}

/// A hit returned by [`ImplicitTimelineTree::element_at_sample`].
///
/// Carries the element's hash, the shared `Arc` to the element payload, the
/// in-element offset (interpretation per element kind — see
/// [data-model.md § Temporal query](../design/data-model.md#temporal-query)), and the
/// hash of the element immediately preceding the hit (`None` if the hit is
/// the first element on the track — the natural `Location::Start` anchor
/// for a delta that records an update or delete at this position).
#[derive(Debug, Clone)]
pub struct ElementHit<T> {
    /// Hash of the hit element.
    pub hash: Hash,
    /// Shared pointer to the hit element.
    pub element: Arc<T>,
    /// Offset within the element, in project-rate samples. Per-kind
    /// interpretation lives outside the tree (turn.rs / label.rs).
    pub in_offset: i64,
    /// Hash of the element immediately before the hit; `None` if first.
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

/// Private node — implementation detail of the tree.
struct Node<T: Tilable> {
    hash:             Hash,
    element:          Arc<T>,
    left:             Option<Arc<Node<T>>>,
    right:            Option<Arc<Node<T>>>,
    /// Σ `total_duration()` over the left subtree, in project-rate samples.
    /// Derived; recomputed along the copied path on every edit.
    left_subtree_sum: i64,
    /// AVL balance factor (1 for a leaf). Derived; recomputed on every edit.
    height:           u8,
}

/// Duration-weighted, sequence-ordered AVL with structural sharing.
///
/// Clone is cheap (one `Arc` refcount bump). All mutation methods take
/// `&self` and return a new tree; the prior root remains valid for snapshot
/// / undo. See [data-model.md § Implicit timeline tree](../design/data-model.md#implicit-timeline-tree).
#[derive(Clone)]
pub struct ImplicitTimelineTree<T: Tilable> {
    root: Option<Arc<Node<T>>>,
}

impl<T: Tilable> ImplicitTimelineTree<T> {
    /// Empty tree (no elements, total duration 0).
    pub fn new() -> Self;

    /// O(n) build from an ordered `Vec` of `(hash, element)` pairs in
    /// timeline order. Input is consumed; the resulting AVL is balanced
    /// with `left_subtree_sum` and `height` computed bottom-up.
    pub fn from_sorted_elements(elements: Vec<(Hash, Arc<T>)>) -> Self;

    /// `true` when this tree has no elements.
    pub fn is_empty(&self) -> bool;

    /// Number of elements in the tree.
    pub fn len(&self) -> usize;

    /// Sum of `total_duration()` over all elements (= total track length
    /// in samples).
    pub fn total_duration(&self) -> i64;

    /// In-order iterator yielding [`ElementRef`] (start_sample + hash + element).
    pub fn iter(&self) -> impl Iterator<Item = ElementRef<'_, T>>;

    /// Insert `element` at boundary sample `at_sample`.
    ///
    /// Valid boundaries:
    /// - `0` — insert as new head (always valid, including on an empty tree).
    /// - `total_duration()` — append as new tail.
    /// - The start sample of any existing element — insert before it.
    ///
    /// # Errors
    /// - [`TreeError::SampleOutOfRange`] if `at_sample < 0` or `at_sample > total_duration()`.
    /// - [`TreeError::SampleNotOnBoundary`] if `at_sample` falls inside an
    ///   element's interior (`0 < in_offset < element.total_duration()`).
    pub fn insert_at(
        &self,
        at_sample: i64,
        element_hash: Hash,
        element: Arc<T>,
    ) -> Result<Self, TreeError>;

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
    ) -> Result<Self, TreeError>;

    /// Delete the element whose interval contains `sample`.
    ///
    /// # Errors
    /// [`TreeError::SampleOutOfRange`] if `sample < 0` or `sample >= total_duration()`
    /// (including any call on an empty tree).
    pub fn delete_at(&self, sample: i64) -> Result<Self, TreeError>;

    /// Locate the element covering project sample `t` (≥ 0). Returns
    /// `None` if `t` is past the end of the track (`t >= total_duration()`)
    /// or if the tree is empty.
    ///
    /// O(log n). The returned [`ElementHit::in_offset`] is non-negative
    /// and `< element.total_duration()`. Per-kind interpretation
    /// (in-speech vs. post-turn silence vs. inter-label gap) is the
    /// caller's job.
    pub fn element_at_sample(&self, t: i64) -> Option<ElementHit<T>>;
}

impl<T: Tilable> Default for ImplicitTimelineTree<T> {
    fn default() -> Self { Self::new() }
}

impl<T: Tilable> PartialEq for ImplicitTimelineTree<T> {
    /// Sequence-equality: same length, same `(hash, total_duration)` at
    /// each in-order position. Does NOT compare AVL shape.
    fn eq(&self, other: &Self) -> bool { /* zip in-order iters */ }
}
```

### Implementation notes

These are not API decisions, just the standard recipe — included so
the implementer doesn't reinvent the AVL shape.

- **Node construction helper:** a private `Node::leaf(hash,
  element)` returns a leaf with `height = 1`, `left_subtree_sum =
  0`. A private `Node::balanced(left, hash, element, right)`
  recomputes `left_subtree_sum = subtree_sum(&left)`, `height = 1 +
  max(left.height(), right.height())`, rotates if `|left.height() -
  right.height()| > 1`, and returns an `Arc<Node<T>>`.

- **Subtree-sum / height accessors:** `fn subtree_sum(node:
  &Option<Arc<Node<T>>>) -> i64` returns `0` for `None` and
  `node.left_subtree_sum + node.element.total_duration() +
  subtree_sum(&node.right)` for `Some`. The recursive
  right-subtree call is O(log n) per query — acceptable for the
  ~once-per-balance call rate but pulls O(log n) into every
  rebalance step. The **recommended optimization** is to store
  `total_subtree_sum: i64` as a third augmentation field; then
  `subtree_sum` is O(1) (a single field read), and
  `left_subtree_sum` becomes derivable as
  `left.map_or(0, |l| l.total_subtree_sum)`. If this is taken,
  update the data-model.md `Node<T>` sketch alongside; if not,
  the simpler `(left_subtree_sum, height)` shape is also valid.

- **AVL rotations:** standard single/double left/right rotations.
  Every rebuild goes through `Node::balanced`, which is the only
  place augmentation is recomputed — so rotations cannot leave
  stale `left_subtree_sum` / `height` values.

- **`insert_at`:** the implementer's natural recipe is a single
  recursive descent that mirrors `element_at_sample`'s sample-walk,
  recording the path as it goes. At each node:
  - If `at_sample < left_subtree_sum`: recurse left.
  - Else if `at_sample == left_subtree_sum`: this is the boundary
    "before this node's element" — insert as the rightmost
    descendant of the left subtree (or as a new left child if the
    left subtree is empty), rebuild the path via `Node::balanced`.
  - Else if `at_sample < left_subtree_sum +
    element.total_duration()`: the sample is in this element's
    interior → `Err(SampleNotOnBoundary { sample: at_sample,
    in_element_offset: at_sample - left_subtree_sum })`.
  - Else: recurse right with `at_sample -= left_subtree_sum +
    element.total_duration()`.
  - At the rightmost leaf, `at_sample == 0` means "append here"
    (insert as a new right child or rightmost descendant).
  Edge cases:
  - Empty tree, `at_sample == 0`: leaf of just the new element.
  - Empty tree, `at_sample != 0`: `Err(SampleOutOfRange(at_sample))`.
  - Non-empty, `at_sample == total_duration()`: append at tail
    (recurse always-right until the right subtree is `None`).

- **`update_at` / `delete_at`:** standard recursive descent to find
  the element containing `sample` (same walk as
  `element_at_sample`), then rebuild the path. Delete uses the
  standard AVL splice (replace with in-order successor if the
  deleted node has two children); both rebuild the path via
  `Node::balanced` from the leaves up. No predecessor-tracking
  needed — these ops act on the located element directly.

- **`element_at_sample`:** iterative descent per
  [data-model.md § Temporal query](../design/data-model.md#temporal-query),
  also tracking the predecessor as we go. The predecessor is "the
  last node from which we descended to the right" — if we ever
  descend right from a node, that node becomes the running
  predecessor candidate; we never overwrite it when we descend
  left. At the hit node, if the hit has a left subtree, the actual
  predecessor is the rightmost descendant of that left subtree;
  otherwise it's the running candidate. `None` if the hit is the
  in-order first element.

- **Bulk-build:** recursive divide-and-conquer on a slice:
  `build(slice)` picks `mid = slice.len() / 2`, builds left from
  `slice[..mid]`, takes `slice[mid]` as the root, builds right
  from `slice[mid+1..]`. Augmentation is computed bottom-up as
  nodes are constructed; no rebalancing needed (a midpoint split
  is balanced by construction).

- **`iter`:** an in-order iterator implemented with an explicit
  `Vec` stack (no recursion limit). At each step the iterator
  yields the next in-order node along with the **running prefix
  sum** of `total_duration()` it has accumulated so far — that
  prefix sum is the yielded element's `start_sample`. Used by
  `len`, `PartialEq`, the renderer, and tests.

### Revised: `core/src/project/mod.rs`

Add `pub mod tree;` (sibling to the existing `pub mod hash;`,
`pub mod label;`, `pub mod tilable;`, `pub mod turn;`).

### Reuse from existing code

- [`super::hash::Hash`](../src-tauri/core/src/project/hash.rs) — the
  16-byte content hash carried by each `Node<T>`.
- [`super::tilable::Tilable`](../src-tauri/core/src/project/tilable.rs)
  — the one-method trait bound on `T`.
- [`super::turn::Turn`](../src-tauri/core/src/project/turn.rs) —
  tests instantiate `ImplicitTimelineTree<Turn>`.
- [`super::label::Label`](../src-tauri/core/src/project/label.rs) —
  tests instantiate `ImplicitTimelineTree<Label>`.
- `std::sync::Arc` — node sharing.
- **No new dependencies.** No schema changes. No blob-store calls.

## Test plan

All tests inline `#[cfg(test)] mod tests` in `tree.rs`. Tests cover
**both** `ImplicitTimelineTree<Turn>` (the speech-track case,
exercising the in-speech vs. post-turn-silence in-offset distinction
in the temporal-query consumer) and `ImplicitTimelineTree<Label>`
(the labels track, with strictly inter-element-gap semantics) per
[phase1-m1.md § Step 6 Verify](phase1-m1.md#step-6--implicit-timeline-tree-projecttreers).

### Shared helpers

```rust
// Tiny Turn / Label factories that take only the duration fields the tests
// care about — the other fields stay constant. The Tilable impl is the only
// thing the tree depends on, so the factories don't need to populate words,
// splices, or text meaningfully.
fn turn_with(id: u64, turn_duration: i64, post_turn_silence: i64) -> (Hash, Arc<Turn>) {
    let t = Turn {
        id, speaker_id: None, turn_duration, post_turn_silence,
        words: vec![], splices: vec![],
    };
    let (h, _) = super::turn::store_turn(&t).unwrap();
    (h, Arc::new(t))
}

fn label_with(id: u64, post_label_silence: i64) -> (Hash, Arc<Label>) {
    let l = Label {
        id, text: String::new(), kind: LabelKind::Plain, post_label_silence,
    };
    let (h, _) = super::label::store_label(&l).unwrap();
    (h, Arc::new(l))
}
```

### Tests — structural

T1. **`new_is_empty`** — `ImplicitTimelineTree::<Turn>::new()` has
    `is_empty() == true`, `len() == 0`, `total_duration() == 0`,
    `iter().next().is_none()`. Same for `Label`.

T2. **`bulk_build_empty`** — `from_sorted_elements(vec![])` is the
    same as `new()` (same `len`, same `is_empty`).

T3. **`bulk_build_single`** — `from_sorted_elements(vec![turn_with(1,
    100, 50)])` has `len() == 1`, `total_duration() == 150`, iterator
    yields exactly one `ElementRef` with `start_sample == 0`. Same
    for `Label`.

T4. **`bulk_build_many_preserves_order`** — bulk-build 100 turns
    with distinct ids/durations; `iter().map(|e| e.hash).collect()`
    equals the input hash order; `iter()`'s start samples form the
    correct prefix-sum sequence; `total_duration()` equals the sum.
    Repeat for `Label`.

T5. **`bulk_build_is_height_balanced`** — bulk-build N elements
    (N ∈ {1, 2, 7, 100, 1000}); walk the tree, asserting at every
    node that `|left.height() - right.height()| <= 1`. Repeat for
    Turn and Label.

T6. **`augmentation_invariant_after_bulk_build`** — for N ∈ {1, 7,
    100}, walk every node and verify `node.left_subtree_sum ==
    sum_of_total_duration(node.left)` and `node.height == 1 +
    max(left.height(), right.height())`. (Reaches into the private
    `Node` via the in-module test access.)

T7. **`incremental_append_via_insert_at_total_duration`** — start
    from `new()`, insert 100 turns via `insert_at(total_duration(),
    …)` (append). After each insert: (a) `iter()` yields the
    elements in insertion order with correct start samples;
    (b) `total_duration()` matches; (c) the height-balance and
    augmentation invariants (T5, T6) hold.

T8. **`bulk_build_equals_incremental_append`** — build the same 100
    elements two ways: once via `from_sorted_elements`, once via 100
    sequential `insert_at(total_duration(), …)` calls. Assert
    `bulk_tree == incremental_tree` (the `PartialEq` impl —
    sequence-equality). Repeat for Label.

T9. **`incremental_prepend_via_insert_at_zero`** — start from
    `new()`, insert 50 turns via `insert_at(0, …)` each time. After
    each insert, the just-inserted element is the head (iter's first
    yield has it with `start_sample == 0`). After all inserts,
    iteration order is the reverse of insertion order.

T10. **`incremental_insert_at_random_boundaries`** —
     deterministic seeded random driver: build a 50-element bulk
     tree; perform 200 insert-at-random-boundary iterations (each
     boundary chosen from the current set of element starts and
     `total_duration()`). After each insert: (a) height balance
     at every node, (b) augmentation correctness at every node,
     (c) iter order matches the expected sequence (the test mirrors
     state in a `Vec<Hash>` and asserts equality).

T11. **`random_deletes_preserve_avl_invariant`** — bulk-build a
     200-element tree; deterministic seeded random driver picks a
     sample within a random surviving element and calls
     `delete_at`. After each delete: same three invariants as T10
     (height balance, augmentation, iter order matches mirror).

T12. **`random_mixed_inserts_updates_deletes_preserve_avl_invariant`**
     — 500 iterations of randomly chosen valid `insert_at` /
     `update_at` / `delete_at`. Same invariants checked after each
     op, against the mirror `Vec<Hash>`.

T13. **`structural_sharing_preserved`** — bulk-build a 50-element
     tree `t0`. Insert one element at a mid-tree boundary to get
     `t1`. Verify (a) `t0` is still queryable and yields its
     original 50 elements; (b) at least one untouched subtree is
     shared by `Arc::ptr_eq` between the two trees. (Pick a node
     deep in the opposite side of the tree from the edit point and
     assert pointer equality with the corresponding `t0` node.)

T14. **`update_at_replaces_element_in_place`** — bulk-build {a, b,
     c} (each duration 100); `update_at(150, new_hash, new_arc)`
     (sample 150 falls in b) yields `{a, new, c}` with the same
     total_duration; original tree unchanged. Iterator on the new
     tree yields `new_hash` at `start_sample == 100`.

T15. **`delete_at_removes_element`** — bulk-build {a, b, c} (each
     duration 100); `delete_at(50)` yields `{b, c}`; `delete_at(150)`
     yields `{a, c}`; `delete_at(250)` yields `{a, b}`; original
     tree unchanged in all cases.

T16. **`insert_at_zero_is_new_head`** — bulk-build {a, b} (each
     duration 100); `insert_at(0, x, …)` yields `{x, a, b}` with
     iter start samples `[0, x.dur, x.dur + 100]`.

T17. **`insert_at_total_duration_appends`** — bulk-build {a, b};
     `insert_at(200, x, …)` (where 200 == total_duration) yields
     `{a, b, x}` with iter start samples `[0, 100, 200]`.

T18. **`insert_at_interior_boundary`** — bulk-build {a, b, c}
     (each duration 100); `insert_at(100, x, …)` (boundary between
     a and b) yields `{a, x, b, c}`; `insert_at(200, x, …)`
     (boundary between b and c) yields `{a, b, x, c}`.

T19. **`insert_at_into_empty_tree`** — `new().insert_at(0, x, …)`
     yields a tree of just `x`; `new().insert_at(1, x, …)` returns
     `Err(SampleOutOfRange(1))`.

T20. **`insert_at_interior_sample_errors`** — bulk-build {a, b}
     where a.total_duration == 100, b.total_duration == 100;
     `insert_at(50, x, …)` returns `Err(SampleNotOnBoundary {
     sample: 50, in_element_offset: 50 })`. `insert_at(150, x, …)`
     returns `Err(SampleNotOnBoundary { sample: 150,
     in_element_offset: 50 })`.

T21. **`insert_at_negative_sample_errors`** — `insert_at(-1, x, …)`
     on any tree returns `Err(SampleOutOfRange(-1))`.

T22. **`insert_at_past_total_duration_errors`** — bulk-build
     {a, b} (total 200); `insert_at(201, x, …)` returns
     `Err(SampleOutOfRange(201))`. Edge: exactly `total_duration`
     is the append case (T17), valid.

T23. **`update_at_empty_tree_errors`** — `new().update_at(0, x, …)`
     returns `Err(SampleOutOfRange(0))`.

T24. **`update_at_past_end_errors`** — bulk-build {a, b}
     (total 200); `update_at(200, x, …)` returns
     `Err(SampleOutOfRange(200))` (one-past-the-last-sample).

T25. **`update_at_negative_sample_errors`** — `update_at(-1, x, …)`
     returns `Err(SampleOutOfRange(-1))`.

T26. **`delete_at_empty_tree_errors`** — `new().delete_at(0)`
     returns `Err(SampleOutOfRange(0))`.

T27. **`delete_at_past_end_errors`** — bulk-build {a, b};
     `delete_at(200)` returns `Err(SampleOutOfRange(200))`.

T28. **`delete_until_empty_round_trips_to_new`** — bulk-build
     {a, b, c}; `delete_at(0)` three times; result equals `new()`.

### Tests — temporal queries

T29. **`element_at_sample_empty_tree`** —
     `new().element_at_sample(0)` is `None`. Same for any positive
     `t`. Same for `t == -1` (negative samples are out of range).

T30. **`element_at_sample_zero_is_first`** — for a non-empty tree,
     `element_at_sample(0)` returns the head element with
     `in_offset == 0` and `predecessor == None`.

T31. **`element_at_sample_last_sample`** — tree of three elements
     each `total_duration() == 100`; `element_at_sample(299)`
     returns the third element with `in_offset == 99` and
     `predecessor == Some(second_hash)`.

T32. **`element_at_sample_past_end_is_none`** — same tree;
     `element_at_sample(300)` is `None`. `element_at_sample(i64::MAX)`
     is `None`.

T33. **`element_at_sample_lands_in_each_element`** — bulk-build 50
     elements of distinct durations; for each element, query at its
     start sample and `start_sample + total_duration / 2`; assert
     the hit is the correct element with the correct in-offset and
     correct predecessor (`None` for the first, prior element's
     hash otherwise).

T34. **`element_at_sample_turn_offset_distinguishes_speech_vs_silence`**
     — a `Turn` with `turn_duration = 100`, `post_turn_silence = 50`;
     query at `t = 50`: hit's in-offset 50 (`< turn_duration` →
     caller-side interpretation: in speech). Query at `t = 120`:
     same turn, in-offset 120 (`>= turn_duration` → caller-side
     interpretation: in post-silence). The tree returns the same
     `ElementHit` shape both times; this test pins that the
     in-offset arithmetic is correct.

T35. **`element_at_sample_label_offset_is_inter_label_gap`** — a
     `Label` with `post_label_silence = 100`; query at `t = 50`
     hits the label with in-offset 50. (Trivially identical
     structure to Turn — pins that the generic tree behavior is
     element-kind agnostic.)

T36. **`element_at_sample_logn_with_balanced_tree`** — sanity
     check: bulk-build 10_000 elements (each duration 100);
     `element_at_sample` at five widely-spaced positions returns
     the correct elements. (No actual timing assertion — just
     confirms descent terminates and is correct on a deep tree.)

### Tests — iteration with start samples

T37. **`iter_start_samples_are_running_prefix_sum`** — bulk-build
     50 elements with varied durations; verify
     `iter().map(|e| e.start_sample).collect::<Vec<_>>()` equals
     the prefix-sum of `iter().map(|e| e.element.total_duration())`,
     starting at 0. Repeat for Turn and Label.

T38. **`iter_after_random_edits_matches_mirror`** — already
     covered by T10–T12's mirror-equality checks; this test pins
     the specific property that `iter()` yields `(start_sample,
     hash)` pairs in order matching the mirror's running prefix
     sum.

### Tests — sequence equality

T39. **`eq_compares_in_order_sequence_not_shape`** — bulk-build
     7 turns into `tree_a`; insert the same 7 turns sequentially
     (each via `insert_at(total_duration(), …)`) into `tree_b`.
     `tree_a == tree_b` is `true` even though their AVL shapes
     likely differ.

T40. **`eq_distinguishes_different_sequences`** — bulk-build
     {a, b, c}; bulk-build {a, c, b}. Not equal.

T41. **`eq_distinguishes_different_durations`** — two trees of one
     element each with the same hash but different `total_duration`
     (different element payload via the factory). Not equal. (Pins
     that the comparison covers durations, not just hashes — a
     defensive check against hash collisions on differently-tiled
     elements.)

### Out-of-scope tests (covered elsewhere or in later steps)

- **Delta application** — Step 7. Tree primitives are tested
  directly here; the loop that turns `Vec<Delta>` into an
  adjacency-list mutation pass is a Step 7 concern. Tree mutation
  primitives are not on the replay path.
- **Snapshot replay** — Step 8. The "replay produces the same tree
  as the pre-snapshot tree" equivalence test lives in Step 8,
  using `PartialEq` from this step.
- **Undo / redo** — Step 10. The "prior root `Arc` retained for
  undo" property is structurally guaranteed by this step's
  immutability (each edit returns a new tree); the actual undo
  stack lives in `undo.rs`.
- **Engine integration** — Step 11. `ProjectState` owns a
  `BTreeMap<u32, …>`; that wiring is Step 11's.
- **Performance / large-N microbenchmarks** — out of scope for M1.
  The O(log n) temporal query and O(n) bulk-build are verified
  algorithmically, not by timing.
- **Multithread safety** — `Arc<Node<T>>` is `Send + Sync` when
  `T: Send + Sync`. The snapshot writer (Step 11) clones the root
  `Arc` and serializes off-thread; the tree itself does no
  locking. No test in Step 6 — Step 11 carries the threading test.

## Documentation touches

- **`data-model.md` — small updates:**
  1. Add `hash: Hash` to the illustrative `struct Node<T>` in
     [§ Implicit timeline tree](../design/data-model.md#implicit-timeline-tree)
     (line ~95) with a one-line comment explaining why (the
     snapshot blob and delta `Location` reference elements by hash;
     carrying it on the node lets iteration surface it without
     re-serializing).
  2. **Remove or reframe the "inverse query (absolute start sample
     of a given element)" sentence** in
     [§ Temporal query](../design/data-model.md#temporal-query). With the
     tree's identity for mutation moved to sample position and the
     `iter()` method exposing running start samples directly, there
     is no longer a need to look up "the start sample of element
     *H*" through the tree. The renderer caches start samples
     during render-time iteration; commands address edits by
     sample, not by hash. Replace the sentence with something like:
     > Renderers obtain each element's start sample by iterating
     > the tree once and accumulating `total_duration()` as they
     > go (`iter()` yields it natively). The tree does not provide
     > a hash-keyed inverse query — the implicit ordering is by
     > timeline position, not by hash, so any such query would be
     > O(n) with no in-engine consumer.

  Both edits land in the same commit as the implementation.

- **`phase1-m1.md` Step 6 prose:** the existing bullets still read
  correctly under the affected-element-keyed shape (they describe
  "AVL insert/update/delete" generically; the wording does not
  prescribe predecessor-based addressing). One small clarification
  is worth adding to the "Verify" line — drop "`start_sample_of`
  inverts `element_at_sample`" and replace with "iteration yields
  start samples consistent with the prefix-sum of
  `total_duration()`" — to match the actual API.

- **No conventions.md edit needed.** The tree's API conforms to
  existing C1 (no `unwrap` / `expect` / `panic`), B1 (clippy
  clean), E2 (`#![warn(missing_docs)]`) without exception.

## Out of scope for Step 6

- **The `Location` enum (`Start` | `After(Hash)`)** and the delta
  apply / inverse machinery — Step 7. The journal layer keeps
  hash-keyed addressing (per the persistence invariants in
  [data-model.md § Deltas](../design/data-model.md#deltas)); tree mutation
  is sample-keyed because the tree is in-memory and
  position-ordered. The two layers convert at edit time, not on
  replay.
- **`Delta` / `DeltaOp`** — Step 7.
- **`Snapshot` struct / flatten / unflatten** — Step 8.
  `Snapshot::from_tree(tree)` flattens
  `tree.iter().map(|e| e.hash).collect::<Vec<_>>()`.
- **Journal interaction** — Step 9.
- **Undo / redo stacks** — Step 10.
- **`ProjectState` engine** — Step 11.
- **Per-kind in-element offset classification helpers** — small
  free functions in `turn.rs` / `label.rs` per
  [phase1-m1.md § Step 6](phase1-m1.md#step-6--implicit-timeline-tree-projecttreers).
  Step 6 ships `ElementHit.in_offset` as a raw `i64`; the helpers
  that classify it as "in-speech vs. post-turn-silence" (Turn) or
  "inter-label gap" (Label) can land here (as small free functions)
  or in Step 8 — either is fine. **The recommendation is to land
  them in this step's commit** since they are trivial (two-line
  `if offset < turn.turn_duration { … }` for Turn; identity for
  Label) and tests T34 / T35 anchor them. If kept out of Step 6,
  T34 / T35 assert on `ElementHit.in_offset` numerically (which
  they already do — typed wrappers would be an additional
  affordance, not a test requirement).
- **A side hash → position index** to make hash-keyed queries
  O(log n) — not added; no in-engine consumer needs hash-keyed
  queries. The few edit-time uses of a hash (computing
  `Location::After(predecessor)` for an insert delta) are
  satisfied by `element_at_sample(at_sample - 1).hash`, O(log n).
- **`Drop` impl, arena, free list, index remapping** — none
  needed (`Arc` reference counting handles everything).

## Verification

- `cargo fmt --check` from `src-tauri/`.
- `cargo clippy -p core -- -D warnings` (must remain green with
  `unwrap_used`, `expect_used`, `panic`, `missing_docs`,
  `cognitive_complexity`, and `too_many_lines` all CI-gated). The
  AVL rebalance helper is the likeliest cognitive-complexity
  offender — split into `rotate_left` / `rotate_right` / `balanced`
  helpers early to stay under the threshold.
- `cargo test -p core project::tree::` — runs the ~41 tests above.
- `cargo test -p core project::` — confirms no regression against
  the existing `hash::`, `turn::`, `label::`, `tilable` tests
  (the `pub mod tree;` addition is purely additive).
- `cargo test -p core` — confirms no regression elsewhere
  (`db::`, `settings`, `ipc`, `task`, `audio` modules are
  untouched).
- `cargo test --workspace` — confirms the broader workspace is
  green (the `proto` and `app` crates do not reference `tree.rs`).
- **One commit on `claude/1M1`, unsigned** per the GPG-by-branch
  policy in [CLAUDE.md](../CLAUDE.md). Subject:
  `1M1-06: implicit timeline tree (sample-keyed AVL with structural sharing)`.
  The commit bundles `core/src/project/tree.rs`, the `pub mod
  tree;` addition in `core/src/project/mod.rs`, and the
  data-model.md edits to `struct Node<T>` and the inverse-query
  paragraph. If the optional `classify_offset` helpers are
  included, they land in the same commit (small one-liners on
  `turn.rs` / `label.rs`).

## Downstream implications (flag for later steps)

- **Step 7 (`delta.rs`):** introduces `Location { Start,
  After(Hash) }` and `DeltaOp { InsertAfter, UpdateAfter,
  DeleteAfter }`. Delta apply walks `Vec<Delta>` against a
  working **adjacency list** (`HashMap<Option<Hash>, Hash>` keyed
  by predecessor, plus a `Start → first_hash` edge) — **not** the
  tree. This is the only place hash-keyed addressing is needed on
  the load path, and the adjacency list is the right data
  structure for it (O(1) per delta). Delta inverse is computed by
  snapshotting the pre-edit successor (`InsertAfter` ↔
  `DeleteAfter`, `UpdateAfter h_new` ↔ `UpdateAfter h_old`); the
  tree itself is unchanged by inverse computation. **The tree's
  sample-keyed primitives are used only at edit time** (M4/M5
  commands), when the edit's sample anchor is known directly from
  the UI.

- **Step 8 (`snapshot.rs`):** `Snapshot::from_tree(tree) ->
  Snapshot` is a one-liner using
  `tree.iter().map(|e| e.hash).collect()`. The reverse — building
  a tree from a hash sequence — uses `from_sorted_elements` after
  loading each element via `store::get` + `load_turn` /
  `load_label`. The replay-equivalence test that "the trees from
  replay equal the trees captured before the save" leans on this
  step's `PartialEq` (sequence equality).

- **Step 10 (`undo.rs`):** the undo stack retains the prior root
  `Arc<Node<T>>` (effectively, the prior
  `ImplicitTimelineTree<T>` since `Clone` is one refcount bump).
  Undo is "set `current_tree = retained_prior`"; memory cost is
  only the changed path that uniquely-owns its nodes.

- **Step 11 (`engine.rs`):** `ProjectState` holds
  `BTreeMap<u32, …>` for per-track trees. Track 0 is
  `ImplicitTimelineTree<Label>`; others are
  `ImplicitTimelineTree<Turn>`. Because the map's value type
  varies by track-id, the natural shape is two separate maps or
  an enum per track: `enum TrackTree {
  Labels(ImplicitTimelineTree<Label>),
  Speech(ImplicitTimelineTree<Turn>) }`. The exact shape is a
  Step 11 decision; Step 6 only ships the generic tree.

- **M4 / M5 (turn-mutating commands):** when actual edit commands
  land, each command:
  1. Receives an edit sample `s` from the UI.
  2. Queries `tree.element_at_sample(s)` to learn the affected
     element's hash and the predecessor's hash.
  3. Computes new element(s) and their hashes via
     `store_turn` / `store_label`.
  4. Calls `tree.insert_at(b, …)` / `update_at(s, …)` /
     `delete_at(s)` to produce the new tree.
  5. Records a `Vec<Delta>` for the journal with `Location::Start`
     or `Location::After(predecessor_hash)` (where
     `predecessor_hash` came from step 2, or from a separate
     `element_at_sample(b - 1).hash` query for an insert delta).
  Steps 4 and 5 are sibling outputs of the same edit pass; neither
  drives the other.

- **Phase 6 scripting / plugin host (post-M1):** the tree's public
  surface (`ImplicitTimelineTree`, `ElementRef`, `ElementHit`,
  `TreeError`) is the natural read-only API for scripts that want
  to walk the timeline. Mutation should remain command-driven, not
  direct.
