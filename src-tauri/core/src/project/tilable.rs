//! Tilable: the contract every tree-element type implements so the implicit
//! timeline tree can be generic over Turn / Label.

/// Total contribution of this element to its track's timeline, in project-rate samples.
///
/// Used by the tree's `left_subtree_sum` augmentation and the temporal-query
/// advance step. See [data-model.md § Tilable trait](data-model.md#tilable-trait).
pub trait Tilable {
    /// Returns the element's total contribution to the timeline, in samples.
    fn total_duration(&self) -> i64;
}
