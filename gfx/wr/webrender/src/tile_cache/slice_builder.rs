/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use api::{BorderRadius, ClipId, ClipMode, ColorF, DebugFlags, PrimitiveFlags, QualitySettings, RasterSpace};
use api::units::*;
use crate::clip::{clamped_radius, ClipItemKeyKind, ClipNodeId, ClipTreeBuilder, intersect_rounded_rects};
use crate::frame_builder::FrameBuilderConfig;
use crate::internal_types::FastHashMap;
use crate::picture::{PrimitiveList, PictureInstance, Picture3DContext, PictureFlags};
use crate::picture_composite_mode::PictureCompositeMode;
use crate::tile_cache::{SliceId, TileCacheParams};
use crate::prim_store::{PrimitiveInstance, PrimitiveKind, PrimitiveStore, PictureIndex};
use crate::scene_building::SliceFlags;
use crate::scene_builder_thread::Interners;
use crate::spatial_tree::{SpatialNodeIndex, SceneSpatialTree};
use crate::util::{MaxRect, VecHelper};
use std::mem;

/*
 Types and functionality related to picture caching. In future, we'll
 move more and more of the existing functionality out of picture.rs
 and into here.
 */

// If the page would create too many slices (an arbitrary definition where
// it's assumed the GPU memory + compositing overhead would be too high)
// then create a single picture cache for the remaining content. This at
// least means that we can cache small content changes efficiently when
// scrolling isn't occurring. Scrolling regions will be handled reasonably
// efficiently by the dirty rect tracking (since it's likely that if the
// page has so many slices there isn't a single major scroll region).
const MAX_CACHE_SLICES: usize = 16;

struct SliceDescriptor {
    prim_list: PrimitiveList,
    scroll_root: SpatialNodeIndex,
}

struct SliceCoverage {
    spatial_node_index: SpatialNodeIndex,
    rect: LayoutRect,
    debug_primitive: Option<String>,
    prim_rect: LayoutRect,
    prim_local_clip_rect: LayoutRect,
}

enum SliceKind {
    Default {
        secondary_slices: Vec<SliceDescriptor>,
    },
    Atomic {
        prim_list: PrimitiveList,
    },
}

impl SliceKind {
    fn default() -> Self {
        SliceKind::Default {
            secondary_slices: Vec::new(),
        }
    }
}

struct PrimarySlice {
    /// Whether this slice is atomic or has secondary slice(s)
    kind: SliceKind,
    /// Optional background color of this slice
    background_color: Option<ColorF>,
    /// Optional root clip for the iframe
    iframe_clip: Option<ClipId>,
    /// Information about how to draw and composite this slice
    slice_flags: SliceFlags,
    coverage: Vec<SliceCoverage>,
    /// This slice contains active backdrop filters and must remain isolated
    /// from ordinary content which overlaps it.
    is_active_backdrop: bool,
}

impl PrimarySlice {
    fn new(
        slice_flags: SliceFlags,
        iframe_clip: Option<ClipId>,
        background_color: Option<ColorF>,
    ) -> Self {
        PrimarySlice {
            kind: SliceKind::default(),
            background_color,
            iframe_clip,
            slice_flags,
            coverage: Vec::new(),
            is_active_backdrop: false,
        }
    }

    fn intersects_coverage(
        &self,
        spatial_node_index: SpatialNodeIndex,
        rect: &LayoutRect,
    ) -> bool {
        if rect.is_empty() {
            return false;
        }

        self.coverage.iter().any(|coverage| {
            coverage.spatial_node_index != spatial_node_index ||
                coverage.rect.intersects(rect)
        })
    }

    fn has_too_many_slices(&self) -> bool {
        match self.kind {
            SliceKind::Atomic { .. } => false,
            SliceKind::Default { ref secondary_slices } => secondary_slices.len() > MAX_CACHE_SLICES,
        }
    }

    fn merge(&mut self) {
        self.slice_flags |= SliceFlags::IS_ATOMIC;

        let old = mem::replace(
            &mut self.kind,
            SliceKind::Default { secondary_slices: Vec::new() },
        );

        self.kind = match old {
            SliceKind::Default { mut secondary_slices } => {
                let mut prim_list = PrimitiveList::empty();

                for descriptor in secondary_slices.drain(..) {
                    prim_list.merge(descriptor.prim_list);
                }

                SliceKind::Atomic {
                    prim_list,
                }
            }
            atomic => atomic,
        }
    }
}

/// Used during scene building to construct the list of pending tile caches.
pub struct TileCacheBuilder {
    /// List of tile caches that have been created so far (last in the list is currently active).
    primary_slices: Vec<PrimarySlice>,
    /// Cache the previous scroll root search for a spatial node, since they are often the same.
    prev_scroll_root_cache: (SpatialNodeIndex, SpatialNodeIndex),
    /// Handle to the root reference frame
    root_spatial_node_index: SpatialNodeIndex,
    /// Debug flags to provide to our TileCacheInstances.
    debug_flags: DebugFlags,
    /// First primary slice in the current hard-barrier group. Active backdrop
    /// layerization may reorder non-overlapping items within this group, but must
    /// never move content across iframe, scrollbar, or other explicit barriers.
    current_group_start: usize,
    /// While a backdrop filter is being constructed, both its hidden filtered
    /// picture and its BackdropRender primitive are forced into this slice.
    active_backdrop_slice: Option<usize>,
    active_backdrop_coverage: Option<(SpatialNodeIndex, LayoutRect)>,
    pending_backdrop_foreground_coverage: Option<(SpatialNodeIndex, LayoutRect)>,
}

/// The output of a tile cache builder, containing all details needed to construct the
/// tile cache(s) for the next scene, and retain tiles from the previous frame when sent
/// send to the frame builder.
pub struct TileCacheConfig {
    /// Mapping of slice id to the parameters needed to construct this tile cache.
    pub tile_caches: FastHashMap<SliceId, TileCacheParams>,
    /// Number of picture cache slices that were created (for profiler)
    pub picture_cache_slice_count: usize,
}

impl TileCacheConfig {
    pub fn new(picture_cache_slice_count: usize) -> Self {
        TileCacheConfig {
            tile_caches: FastHashMap::default(),
            picture_cache_slice_count,
        }
    }
}

impl TileCacheBuilder {
    /// Construct a new tile cache builder.
    pub fn new(
        root_spatial_node_index: SpatialNodeIndex,
        background_color: Option<ColorF>,
        debug_flags: DebugFlags,
    ) -> Self {
        TileCacheBuilder {
            primary_slices: vec![PrimarySlice::new(SliceFlags::empty(), None, background_color)],
            prev_scroll_root_cache: (SpatialNodeIndex::INVALID, SpatialNodeIndex::INVALID),
            root_spatial_node_index,
            debug_flags,
            current_group_start: 0,
            active_backdrop_slice: None,
            active_backdrop_coverage: None,
            pending_backdrop_foreground_coverage: None,
        }
    }

    pub fn make_current_slice_atomic(&mut self) {
        self.primary_slices
            .last_mut()
            .unwrap()
            .merge();
    }

    /// Marks every secondary cache in the current primary slice as potentially
    /// sampled by a later backdrop filter, preventing unsafe promotion and
    /// occlusion of its contents.
    pub fn mark_current_slice_has_cross_slice_backdrop(&mut self) {
        self.primary_slices
            .last_mut()
            .unwrap()
            .slice_flags |= SliceFlags::HAS_CROSS_SLICE_BACKDROP;
    }

    /// Start or reuse an isolated picture-cache slice for a top-level backdrop filter.
    ///
    /// Filters reuse the foremost active slice when it is already above their
    /// backdrop content. Otherwise a new active slice preserves earlier filter
    /// foregrounds while allowing the later filter to sample them.
    pub fn begin_active_backdrop(
        &mut self,
        spatial_node_index: SpatialNodeIndex,
        rect: LayoutRect,
    ) {
        debug_assert!(self.active_backdrop_slice.is_none());
        debug_assert!(self.active_backdrop_coverage.is_none());
        self.active_backdrop_coverage = Some((spatial_node_index, rect));

        let mut highest_intersection = self.current_group_start;
        for index in self.current_group_start + 1..self.primary_slices.len() {
            if let Some(coverage) = self.primary_slices[index]
                .coverage
                .iter()
                .find(|coverage| {
                    coverage.spatial_node_index != spatial_node_index ||
                        coverage.rect.intersects(&rect)
                })
            {
                if std::env::var_os("WR_BACKDROP_DEBUG").is_some() {
                    println!(
                        "BF_INTERSECTION filter_spatial={:?} filter_rect={:?} slice={} reason={} coverage_spatial={:?} coverage_rect={:?} prim_rect={:?} prim_clip={:?} prim={}",
                        spatial_node_index,
                        rect,
                        index,
                        if coverage.spatial_node_index != spatial_node_index {
                            "spatial"
                        } else {
                            "rect"
                        },
                        coverage.spatial_node_index,
                        coverage.rect,
                        coverage.prim_rect,
                        coverage.prim_local_clip_rect,
                        coverage.debug_primitive.as_deref().unwrap_or("unknown"),
                    );
                }
                highest_intersection = index;
            }
        }
        let insertion_index = highest_intersection + 1;
        let reusable_slice_index = self.primary_slices[self.current_group_start..]
            .iter()
            .rposition(|slice| slice.is_active_backdrop)
            .map(|index| self.current_group_start + index);
        let (backdrop_slice_index, reuse_slice) = match reusable_slice_index {
            Some(index) if index >= insertion_index => (index, true),
            Some(_) | None => (insertion_index, false),
        };

        // Any earlier slice may become a resolve source for this filter. Retain
        // those tiles even when the compositor would otherwise consider them
        // hidden behind the active filter or another higher slice.
        for slice in &mut self.primary_slices[..backdrop_slice_index] {
            slice.slice_flags |= SliceFlags::HAS_CROSS_SLICE_BACKDROP;
        }

        if reuse_slice {
            if std::env::var_os("WR_BACKDROP_DEBUG").is_some() {
                println!(
                    "BF_SLICE reuse={} insertion={} spatial={:?} rect={:?}",
                    backdrop_slice_index,
                    insertion_index,
                    spatial_node_index,
                    rect,
                );
            }
            self.primary_slices[backdrop_slice_index]
                .coverage
                .push(SliceCoverage {
                    spatial_node_index,
                    rect,
                    debug_primitive: None,
                    prim_rect: rect,
                    prim_local_clip_rect: rect,
                });
            self.active_backdrop_slice = Some(backdrop_slice_index);
            return;
        }

        if std::env::var_os("WR_BACKDROP_DEBUG").is_some() {
            println!(
                "BF_SLICE create={} spatial={:?} rect={:?}",
                insertion_index,
                spatial_node_index,
                rect,
            );
        }

        let iframe_clip = self.primary_slices[self.current_group_start]
            .iframe_clip
            .clone();
        let mut slice = PrimarySlice::new(
            SliceFlags::IS_ATOMIC | SliceFlags::HAS_CROSS_SLICE_BACKDROP,
            iframe_clip,
            None,
        );
        slice.merge();
        slice.is_active_backdrop = true;
        slice.coverage.push(SliceCoverage {
            spatial_node_index,
            rect,
            debug_primitive: None,
            prim_rect: rect,
            prim_local_clip_rect: rect,
        });
        self.primary_slices.insert(insertion_index, slice);
        self.active_backdrop_slice = Some(insertion_index);
    }

    /// Finish adding the two internal primitives which represent an active
    /// backdrop filter and resume overlap-based placement of ordinary content.
    pub fn end_active_backdrop(&mut self) {
        debug_assert!(self.active_backdrop_slice.is_some());
        self.active_backdrop_slice = None;
        self.pending_backdrop_foreground_coverage = self.active_backdrop_coverage.take();
    }

    /// Returns whether content already added to the current primary slice has
    /// a different scroll root and therefore lives in a cache separate from
    /// the backdrop filter.
    pub fn backdrop_filter_may_sample_cross_slice(
        &self,
        filter_scroll_root: SpatialNodeIndex,
    ) -> bool {
        match self.primary_slices.last().unwrap().kind {
            SliceKind::Default { ref secondary_slices } => secondary_slices
                .iter()
                .any(|slice| slice.scroll_root != filter_scroll_root),
            SliceKind::Atomic { .. } => false,
        }
    }

    /// Returns true if the current slice has no primitives added yet
    pub fn is_current_slice_empty(&self) -> bool {
        self.primary_slices[self.current_group_start..]
            .iter()
            .all(|slice| {
                match slice.kind {
                    SliceKind::Default { ref secondary_slices } => {
                        secondary_slices.is_empty()
                    }
                    SliceKind::Atomic { ref prim_list } => {
                        prim_list.is_empty()
                    }
                }
            })
    }

    /// Set a barrier that forces a new tile cache next time a prim is added.
    pub fn add_tile_cache_barrier(
        &mut self,
        slice_flags: SliceFlags,
        iframe_clip: Option<ClipId>,
    ) {
        debug_assert!(self.active_backdrop_slice.is_none());
        let new_slice = PrimarySlice::new(
            slice_flags,
            iframe_clip,
            None,
        );

        self.primary_slices.push(new_slice);
        self.current_group_start = self.primary_slices.len() - 1;
    }

    /// Create a new tile cache for an existing prim_list
    fn build_tile_cache(
        &mut self,
        prim_list: PrimitiveList,
        spatial_tree: &SceneSpatialTree,
    ) -> Option<SliceDescriptor> {
        if prim_list.is_empty() {
            return None;
        }

        // Iterate the clusters and determine which is the most commonly occurring
        // scroll root. This is a reasonable heuristic to decide which spatial node
        // should be considered the scroll root of this tile cache, in order to
        // minimize the invalidations that occur due to scrolling. It's often the
        // case that a blend container will have only a single scroll root.
        let mut scroll_root_occurrences = FastHashMap::default();

        for cluster in &prim_list.clusters {
            // If we encounter a cluster which has an unknown spatial node,
            // we don't include that in the set of spatial nodes that we
            // are trying to find scroll roots for. Later on, in finalize_picture,
            // the cluster spatial node will be updated to the selected scroll root.
            if cluster.spatial_node_index == SpatialNodeIndex::UNKNOWN {
                continue;
            }

            let scroll_root = find_scroll_root(
                cluster.spatial_node_index,
                &mut self.prev_scroll_root_cache,
                spatial_tree,
                true,
            );

            *scroll_root_occurrences.entry(scroll_root).or_insert(0) += 1;
        }

        // We can't just select the most commonly occurring scroll root in this
        // primitive list. If that is a nested scroll root, there may be
        // primitives in the list that are outside that scroll root, which
        // can cause panics when calculating relative transforms. To ensure
        // this doesn't happen, only retain scroll root candidates that are
        // also ancestors of every other scroll root candidate.
        let scroll_roots: Vec<SpatialNodeIndex> = scroll_root_occurrences
            .keys()
            .cloned()
            .collect();

        scroll_root_occurrences.retain(|parent_spatial_node_index, _| {
            scroll_roots.iter().all(|child_spatial_node_index| {
                parent_spatial_node_index == child_spatial_node_index ||
                spatial_tree.is_ancestor(
                    *parent_spatial_node_index,
                    *child_spatial_node_index,
                )
            })
        });

        // Select the scroll root by finding the most commonly occurring one
        let scroll_root = scroll_root_occurrences
            .iter()
            .max_by_key(|entry | entry.1)
            .map(|(spatial_node_index, _)| *spatial_node_index)
            .unwrap_or(self.root_spatial_node_index);

        Some(SliceDescriptor {
            scroll_root,
            prim_list,
        })
    }

    fn add_prim_to_primary_slice(
        primary_slice: &mut PrimarySlice,
        prev_scroll_root_cache: &mut (SpatialNodeIndex, SpatialNodeIndex),
        root_spatial_node_index: SpatialNodeIndex,
        prim_instance: PrimitiveInstance,
        prim_rect: LayoutRect,
        prim_local_clip_rect: LayoutRect,
        spatial_node_index: SpatialNodeIndex,
        prim_flags: PrimitiveFlags,
        spatial_tree: &SceneSpatialTree,
        quality_settings: &QualitySettings,
        prim_instances: &mut Vec<PrimitiveInstance>,
        clip_tree_builder: &ClipTreeBuilder,
    ) {
        match primary_slice.kind {
            SliceKind::Atomic { ref mut prim_list } => {
                prim_list.add_prim(
                    prim_instance,
                    prim_rect,
                    prim_local_clip_rect,
                    spatial_node_index,
                    prim_flags,
                    prim_instances,
                );
            }
            SliceKind::Default { ref mut secondary_slices } => {
                assert_ne!(spatial_node_index, SpatialNodeIndex::UNKNOWN);

                // Check if we want to create a new slice based on the current / next scroll root
                let scroll_root = find_scroll_root(
                    spatial_node_index,
                    prev_scroll_root_cache,
                    spatial_tree,
                    // Allow sticky frames as scroll roots, unless our quality settings prefer
                    // subpixel AA over performance.
                    !quality_settings.force_subpixel_aa_where_possible,
                );

                let current_scroll_root = secondary_slices
                    .last()
                    .map(|p| p.scroll_root);

                let mut want_new_tile_cache = secondary_slices.is_empty();

                if let Some(current_scroll_root) = current_scroll_root {
                    want_new_tile_cache |= match (current_scroll_root, scroll_root) {
                        (_, _) if current_scroll_root == root_spatial_node_index && scroll_root == root_spatial_node_index => {
                            // Both current slice and this cluster are fixed position, no need to cut
                            false
                        }
                        (_, _) if current_scroll_root == root_spatial_node_index => {
                            // A real scroll root is being established, so create a cache slice
                            true
                        }
                        (_, _) if scroll_root == root_spatial_node_index => {
                            // If quality settings force subpixel AA over performance, skip creating
                            // a slice for the fixed position element(s) here.
                            if quality_settings.force_subpixel_aa_where_possible {
                                false
                            } else {
                                // A fixed position slice is encountered within a scroll root. Only create
                                // a slice in this case if all the clips referenced by this cluster are also
                                // fixed position. There's no real point in creating slices for these cases,
                                // since we'll have to rasterize them as the scrolling clip moves anyway. It
                                // also allows us to retain subpixel AA in these cases. For these types of
                                // slices, the intra-slice dirty rect handling typically works quite well
                                // (a common case is parallax scrolling effects).
                                let mut create_slice = true;

                                let mut current_node_id = prim_instance.clip_node_id;

                                while current_node_id != ClipNodeId::NONE {
                                    let node = clip_tree_builder.get_node(current_node_id);

                                    let spatial_root = find_scroll_root(
                                        node.spatial_node_index,
                                        prev_scroll_root_cache,
                                        spatial_tree,
                                        true,
                                    );

                                    if spatial_root != root_spatial_node_index {
                                        create_slice = false;
                                        break;
                                    }

                                    current_node_id = node.parent;
                                }

                                create_slice
                            }
                        }
                        (curr_scroll_root, scroll_root) => {
                            // Two scrolling roots - only need a new slice if they differ
                            curr_scroll_root != scroll_root
                        }
                    };
                }

                if want_new_tile_cache {
                    secondary_slices.push(SliceDescriptor {
                        prim_list: PrimitiveList::empty(),
                        scroll_root,
                    });
                }

                secondary_slices
                    .last_mut()
                    .unwrap()
                    .prim_list
                    .add_prim(
                        prim_instance,
                        prim_rect,
                        prim_local_clip_rect,
                        spatial_node_index,
                        prim_flags,
                        prim_instances,
                    );
            }
        }
    }

    /// Place ordinary content in the lowest slice that preserves the order of
    /// overlapping content.
    fn select_prim_slice(
        &mut self,
        spatial_node_index: SpatialNodeIndex,
        rect: LayoutRect,
    ) -> usize {
        if let Some(index) = self.active_backdrop_slice {
            return index;
        }

        let mut highest_intersection = None;
        for index in self.current_group_start..self.primary_slices.len() {
            if self.primary_slices[index].intersects_coverage(spatial_node_index, &rect) {
                highest_intersection = Some(index);
            }
        }

        let Some(index) = highest_intersection else {
            return self.current_group_start;
        };

        if !self.primary_slices[index].is_active_backdrop {
            return index;
        }

        let destination = index + 1;
        if destination == self.primary_slices.len() ||
            self.primary_slices[destination].is_active_backdrop
        {
            let iframe_clip = self.primary_slices[index].iframe_clip.clone();
            self.primary_slices.insert(
                destination,
                PrimarySlice::new(SliceFlags::empty(), iframe_clip, None),
            );
        }

        destination
    }

    /// Add a primitive to a picture-cache layer selected by `select_prim_slice`.
    pub fn add_prim(
        &mut self,
        prim_instance: PrimitiveInstance,
        prim_rect: LayoutRect,
        prim_local_clip_rect: LayoutRect,
        spatial_node_index: SpatialNodeIndex,
        prim_flags: PrimitiveFlags,
        spatial_tree: &SceneSpatialTree,
        quality_settings: &QualitySettings,
        prim_instances: &mut Vec<PrimitiveInstance>,
        clip_tree_builder: &ClipTreeBuilder,
    ) {
        let mut coverage_rect = prim_rect
            .intersection(&prim_local_clip_rect)
            .unwrap_or_default();
        if coverage_rect.is_empty() && matches!(prim_instance.kind, PrimitiveKind::Picture { .. }) {
            // Flattened stacking contexts are inserted into their parent with a
            // zero culling rect because their bounds are propagated later during
            // frame building. Until this prototype carries child-picture bounds
            // into the slice builder, treat such a picture as covering the whole
            // coordinate space. Moving it below an active filter could otherwise
            // make the filter sample its own foreground.
            coverage_rect = match self.pending_backdrop_foreground_coverage.take() {
                Some((coverage_spatial_node_index, coverage_rect))
                    if coverage_spatial_node_index == spatial_node_index => coverage_rect,
                Some(_) | None => LayoutRect::max_rect(),
            };
        }

        self.add_prim_with_coverage(
            prim_instance,
            prim_rect,
            prim_local_clip_rect,
            spatial_node_index,
            prim_flags,
            coverage_rect,
            spatial_tree,
            quality_settings,
            prim_instances,
            clip_tree_builder,
        );
    }

    pub fn add_prim_with_coverage(
        &mut self,
        prim_instance: PrimitiveInstance,
        prim_rect: LayoutRect,
        prim_local_clip_rect: LayoutRect,
        spatial_node_index: SpatialNodeIndex,
        prim_flags: PrimitiveFlags,
        coverage_rect: LayoutRect,
        spatial_tree: &SceneSpatialTree,
        quality_settings: &QualitySettings,
        prim_instances: &mut Vec<PrimitiveInstance>,
        clip_tree_builder: &ClipTreeBuilder,
    ) {
        let debug_primitive = if std::env::var_os("WR_BACKDROP_DEBUG").is_some() {
            Some(format!("{:?}", prim_instance.kind))
        } else {
            None
        };
        let primary_slice_index = self.select_prim_slice(
            spatial_node_index,
            coverage_rect,
        );

        if self.active_backdrop_slice.is_none() && !coverage_rect.is_empty() {
            self.primary_slices[primary_slice_index]
                .coverage
                .push(SliceCoverage {
                    spatial_node_index,
                    rect: coverage_rect,
                    debug_primitive,
                    prim_rect,
                    prim_local_clip_rect,
                });
        }

        let root_spatial_node_index = self.root_spatial_node_index;
        Self::add_prim_to_primary_slice(
            &mut self.primary_slices[primary_slice_index],
            &mut self.prev_scroll_root_cache,
            root_spatial_node_index,
            prim_instance,
            prim_rect,
            prim_local_clip_rect,
            spatial_node_index,
            prim_flags,
            spatial_tree,
            quality_settings,
            prim_instances,
            clip_tree_builder,
        );
    }

    /// Consume this object and build the list of tile cache primitives
    pub fn build(
        mut self,
        config: &FrameBuilderConfig,
        prim_store: &mut PrimitiveStore,
        spatial_tree: &SceneSpatialTree,
        prim_instances: &[PrimitiveInstance],
        clip_tree_builder: &mut ClipTreeBuilder,
        interners: &Interners,
    ) -> (TileCacheConfig, Vec<PictureIndex>) {
        if std::env::var_os("WR_BACKDROP_DEBUG").is_some() {
            println!(
                "BF_SLICE_SUMMARY total={} active={}",
                self.primary_slices.len(),
                self.primary_slices.iter().filter(|slice| slice.is_active_backdrop).count(),
            );
        }
        let mut result = TileCacheConfig::new(self.primary_slices.len());
        let mut tile_cache_pictures = Vec::new();
        let primary_slices = std::mem::replace(&mut self.primary_slices, Vec::new());

        for mut primary_slice in primary_slices {

            if primary_slice.has_too_many_slices() {
                primary_slice.merge();
            }

            match primary_slice.kind {
                SliceKind::Atomic { prim_list } => {
                    if let Some(descriptor) = self.build_tile_cache(
                        prim_list,
                        spatial_tree,
                    ) {
                        create_tile_cache(
                            self.debug_flags,
                            primary_slice.slice_flags,
                            descriptor.scroll_root,
                            primary_slice.iframe_clip,
                            descriptor.prim_list,
                            primary_slice.background_color,
                            prim_store,
                            prim_instances,
                            config,
                            &mut result.tile_caches,
                            &mut tile_cache_pictures,
                            clip_tree_builder,
                            interners,
                            spatial_tree,
                        );
                    }
                }
                SliceKind::Default { secondary_slices } => {
                    for descriptor in secondary_slices {
                        create_tile_cache(
                            self.debug_flags,
                            primary_slice.slice_flags,
                            descriptor.scroll_root,
                            primary_slice.iframe_clip,
                            descriptor.prim_list,
                            primary_slice.background_color,
                            prim_store,
                            prim_instances,
                            config,
                            &mut result.tile_caches,
                            &mut tile_cache_pictures,
                            clip_tree_builder,
                            interners,
                            spatial_tree,
                        );
                    }
                }
            }
        }

        (result, tile_cache_pictures)
    }
}

/// Find the scroll root for a given spatial node
fn find_scroll_root(
    spatial_node_index: SpatialNodeIndex,
    prev_scroll_root_cache: &mut (SpatialNodeIndex, SpatialNodeIndex),
    spatial_tree: &SceneSpatialTree,
    allow_sticky_frames: bool,
) -> SpatialNodeIndex {
    if prev_scroll_root_cache.0 == spatial_node_index {
        return prev_scroll_root_cache.1;
    }

    let scroll_root = spatial_tree.find_scroll_root(spatial_node_index, allow_sticky_frames);
    *prev_scroll_root_cache = (spatial_node_index, scroll_root);

    scroll_root
}

/// Given a PrimitiveList and scroll root, construct a tile cache primitive instance
/// that wraps the primitive list.
fn create_tile_cache(
    debug_flags: DebugFlags,
    slice_flags: SliceFlags,
    scroll_root: SpatialNodeIndex,
    iframe_clip: Option<ClipId>,
    prim_list: PrimitiveList,
    background_color: Option<ColorF>,
    prim_store: &mut PrimitiveStore,
    prim_instances: &[PrimitiveInstance],
    frame_builder_config: &FrameBuilderConfig,
    tile_caches: &mut FastHashMap<SliceId, TileCacheParams>,
    tile_cache_pictures: &mut Vec<PictureIndex>,
    clip_tree_builder: &mut ClipTreeBuilder,
    interners: &Interners,
    spatial_tree: &SceneSpatialTree,
) {
    // Accumulate any clip instances from the iframe_clip into the shared clips
    // that will be applied by this tile cache during compositing.
    let mut additional_clips = Vec::new();

    if let Some(clip_id) = iframe_clip {
        additional_clips.push(clip_id);
    }

    // Find the best shared clip node that we can apply while compositing tiles,
    // rather than applying to each item individually.

    // Step 1: Walk the primitive list, and find the LCA of the clip-tree that
    //         matches all primitives. This gives us our "best-case" shared
    //         clip node that moves as many clips as possible to compositing.
    let mut shared_clip_node_id = None;

    for cluster in &prim_list.clusters {
        for prim_instance in &prim_instances[cluster.prim_range()] {
            let node_id = prim_instance.clip_node_id;

            // TODO(gw): Need to cache last clip-node id here?
            shared_clip_node_id = match shared_clip_node_id {
                Some(current) => {
                    Some(clip_tree_builder.find_lowest_common_ancestor(current, node_id))
                }
                None => {
                    Some(node_id)
                }
            }
        }
    }

    // Step 2: Now we need to walk up the shared clip node hierarchy, and remove clips
    //         that we can't handle during compositing, such as:
    //         (a) Non axis-aligned clips
    //         (b) Box-shadow or image-mask clips
    //         (c) More than one rounded-rect clip (unless they can be intersected
    //             into a single rounded-rect clip).
    let mut shared_clip_node_id = shared_clip_node_id.unwrap_or(ClipNodeId::NONE);
    let mut current_node_id = shared_clip_node_id;
    let mut rounded_rect_count = 0;

    // Track accumulated rounded rect info so we can attempt to combine
    // multiple rounded rects into a single compositing clip.
    let mut accumulated_rounded_rect: Option<(LayoutRect, BorderRadius)> = None;

    // SNAPTODO: Scene-build slice partitioning reads `node.unsnapped_clip_rect`
    // (and feeds it through `clamped_radius` / `intersect_rounded_rects` /
    // `accumulated_rounded_rect`) to decide whether clips can be promoted
    // into a shared compositing clip. Snapping isn't available at scene-build
    // time, so audit whether pixel-aligned vs. sub-pixel clip rects can flip
    // the can_use_fast_path / intersect decisions once per-frame snapping
    // is real.
    // Walk up the hierarchy to the root of the clip-tree
    while current_node_id != ClipNodeId::NONE {
        let node = clip_tree_builder.get_node(current_node_id);
        let clip_node_data = &interners.clip[node.handle];

        // Check if this clip is in the root coord system (i.e. is axis-aligned with tile-cache)
        let is_rcs = spatial_tree.is_root_coord_system(node.spatial_node_index);

        let node_valid = if is_rcs {
            match clip_node_data.key.kind {
                ClipItemKeyKind::ImageMask(..) |
                ClipItemKeyKind::Rectangle(ClipMode::ClipOut) |
                ClipItemKeyKind::RoundedRectangle(_, _, ClipMode::ClipOut) => {
                    // Has an image-mask or clip-out clip, we can't handle this as a shared clip
                    false
                }
                ClipItemKeyKind::RoundedRectangle(radius, _, ClipMode::Clip) => {
                    // The shader and CoreAnimation rely on certain constraints such
                    // as uniform radii to be able to apply the clip during compositing.
                    let br = clamped_radius(&BorderRadius::from(radius), node.unsnapped_clip_rect.size());
                    if !debug_flags.contains(DebugFlags::DISABLE_COMPOSITOR_CLIPS) &&
                       br.can_use_fast_path_in(&node.unsnapped_clip_rect) {
                        rounded_rect_count += 1;

                        if accumulated_rounded_rect.is_none() {
                            accumulated_rounded_rect = Some((node.unsnapped_clip_rect, br));
                        }

                        true
                    } else {
                        false
                    }
                }
                ClipItemKeyKind::Rectangle(ClipMode::Clip) => {
                    // We can apply multiple (via combining) axis-aligned rectangle
                    // clips to the shared compositing clip.
                    true
                }
            }
        } else {
            // Has a complex transform, we can't handle this as a shared clip
            false
        };

        if node_valid {
            // This node was found to be one we can apply during compositing.
            if rounded_rect_count > 1 {
                // Check if the two rounded rects can be combined. Both clips are in
                // the root coordinate system (is_rcs). The actual intersection with
                // correct spatial transforms is performed in pre_update; here we just
                // verify the clips are geometrically compatible in their local spaces
                // to decide whether to keep both in the shared clip chain.
                let can_combine = match (accumulated_rounded_rect, clip_node_data.key.kind) {
                    (
                        Some((acc_rect, acc_radius)),
                        ClipItemKeyKind::RoundedRectangle(radius, _, ClipMode::Clip),
                    ) => {
                        let radius = clamped_radius(&BorderRadius::from(radius), node.unsnapped_clip_rect.size());
                        intersect_rounded_rects(
                            acc_rect, acc_radius,
                            node.unsnapped_clip_rect, radius,
                        )
                    }
                    _ => None,
                };

                if let Some((combined_rect, combined_radius)) = can_combine {
                    // Successfully combined — keep both clips in the shared
                    // set and update the accumulated state for potential
                    // further combinations.
                    rounded_rect_count = 1;
                    accumulated_rounded_rect = Some((combined_rect, combined_radius));
                } else {
                    // Can't combine, drop children and keep only this clip.
                    shared_clip_node_id = current_node_id;
                    rounded_rect_count = 1;
                    if let ClipItemKeyKind::RoundedRectangle(radius, _, ClipMode::Clip) = clip_node_data.key.kind {
                        let radius = clamped_radius(&BorderRadius::from(radius), node.unsnapped_clip_rect.size());
                        accumulated_rounded_rect = Some((node.unsnapped_clip_rect, radius));
                    }
                }
            }
        } else {
            // Node was invalid, due to transform / clip type. Drop this clip
            // and reset the rounded rect count to 0, since we drop children
            // from here too.
            shared_clip_node_id = node.parent;
            rounded_rect_count = 0;
            accumulated_rounded_rect = None;
        }

        current_node_id = node.parent;
    }

    let tile_clip_node_id = Some(clip_tree_builder.build_for_tile_cache(
        shared_clip_node_id,
        &additional_clips,
    ));

    // Build a clip-chain for the tile cache, that contains any of the shared clips
    // we will apply when drawing the tiles. In all cases provided by Gecko, these
    // are rectangle clips with a scale/offset transform only, and get handled as
    // a simple local clip rect in the vertex shader. However, this should in theory
    // also work with any complex clips, such as rounded rects and image masks, by
    // producing a clip mask that is applied to the picture cache tiles.

    let slice = tile_cache_pictures.len();

    let background_color = if slice == 0 {
        background_color
    } else {
        None
    };

    let slice_id = SliceId::new(slice);

    // Store some information about the picture cache slice. This is used when we swap the
    // new scene into the frame builder to either reuse existing slices, or create new ones.
    tile_caches.insert(slice_id, TileCacheParams {
        debug_flags,
        slice,
        slice_flags,
        spatial_node_index: scroll_root,
        background_color,
        shared_clip_node_id,
        tile_clip_node_id,
        virtual_surface_size: frame_builder_config.compositor_kind.get_virtual_surface_size(),
        image_surface_count: prim_list.image_surface_count,
        yuv_image_surface_count: prim_list.yuv_image_surface_count,
    });

    let pic_index = prim_store.pictures.alloc().init(PictureInstance::new_image(
        Some(PictureCompositeMode::TileCache { slice_id }),
        Picture3DContext::Out,
        PrimitiveFlags::IS_BACKFACE_VISIBLE,
        prim_list,
        scroll_root,
        RasterSpace::Screen,
        PictureFlags::empty(),
        None,
    ));

    tile_cache_pictures.push(PictureIndex(pic_index as u32));
}

/// Debug information about a set of picture cache slices, exposed via RenderResults
#[derive(Debug)]
#[cfg_attr(feature = "capture", derive(Serialize))]
#[cfg_attr(feature = "replay", derive(Deserialize))]
pub struct PictureCacheDebugInfo {
    pub slices: FastHashMap<usize, SliceDebugInfo>,
}

impl PictureCacheDebugInfo {
    pub fn new() -> Self {
        PictureCacheDebugInfo {
            slices: FastHashMap::default(),
        }
    }

    /// Convenience method to retrieve a given slice. Deliberately panics
    /// if the slice isn't present.
    pub fn slice(&self, slice: usize) -> &SliceDebugInfo {
        &self.slices[&slice]
    }
}

impl Default for PictureCacheDebugInfo {
    fn default() -> PictureCacheDebugInfo {
        PictureCacheDebugInfo::new()
    }
}

/// Debug information about the compositor clip applied to a picture cache slice
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "capture", derive(Serialize))]
#[cfg_attr(feature = "replay", derive(Deserialize))]
pub struct CompositorClipDebugInfo {
    pub rect: DeviceRect,
    pub radius: BorderRadius,
}

/// Debug information about a set of picture cache tiles, exposed via RenderResults
#[derive(Debug)]
#[cfg_attr(feature = "capture", derive(Serialize))]
#[cfg_attr(feature = "replay", derive(Deserialize))]
pub struct SliceDebugInfo {
    pub tiles: FastHashMap<TileOffset, TileDebugInfo>,
    pub compositor_clip: Option<CompositorClipDebugInfo>,
}

impl SliceDebugInfo {
    pub fn new() -> Self {
        SliceDebugInfo {
            tiles: FastHashMap::default(),
            compositor_clip: None,
        }
    }

    /// Convenience method to retrieve a given tile. Deliberately panics
    /// if the tile isn't present.
    pub fn tile(&self, x: i32, y: i32) -> &TileDebugInfo {
        &self.tiles[&TileOffset::new(x, y)]
    }
}

/// Debug information about a tile that was dirty and was rasterized
#[derive(Debug, PartialEq)]
#[cfg_attr(feature = "capture", derive(Serialize))]
#[cfg_attr(feature = "replay", derive(Deserialize))]
pub struct DirtyTileDebugInfo {
    pub local_valid_rect: PictureRect,
    pub local_dirty_rect: PictureRect,
}

/// Debug information about the state of a tile
#[derive(Debug, PartialEq)]
#[cfg_attr(feature = "capture", derive(Serialize))]
#[cfg_attr(feature = "replay", derive(Deserialize))]
pub enum TileDebugInfo {
    /// Tile was occluded by a tile in front of it
    Occluded,
    /// Tile was culled (not visible in current display port)
    Culled,
    /// Tile was valid (no rasterization was done) and visible
    Valid,
    /// Tile was dirty, and was updated
    Dirty(DirtyTileDebugInfo),
}

impl TileDebugInfo {
    pub fn is_occluded(&self) -> bool {
        match self {
            TileDebugInfo::Occluded => true,
            TileDebugInfo::Culled |
            TileDebugInfo::Valid |
            TileDebugInfo::Dirty(..) => false,
        }
    }

    pub fn is_valid(&self) -> bool {
        match self {
            TileDebugInfo::Valid => true,
            TileDebugInfo::Culled |
            TileDebugInfo::Occluded |
            TileDebugInfo::Dirty(..) => false,
        }
    }

    pub fn is_culled(&self) -> bool {
        match self {
            TileDebugInfo::Culled => true,
            TileDebugInfo::Valid |
            TileDebugInfo::Occluded |
            TileDebugInfo::Dirty(..) => false,
        }
    }

    pub fn as_dirty(&self) -> &DirtyTileDebugInfo {
        match self {
            TileDebugInfo::Occluded |
            TileDebugInfo::Culled |
            TileDebugInfo::Valid => {
                panic!("not a dirty tile!");
            }
            TileDebugInfo::Dirty(ref info) => {
                info
            }
        }
    }
}
