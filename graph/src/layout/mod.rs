use std::{
    cmp::Ordering,
    fmt::{Debug, Display, Formatter},
    sync::Arc,
};

use bellframe::{Bell, Block, Mask, Method, PlaceNot, Row, RowBuf, Stage};
use itertools::Itertools;

use crate::{music::MusicType, Graph};

pub mod single_method;

index_vec::define_index_type! { pub struct LinkIdx = usize; }
pub type LinkVec<T> = index_vec::IndexVec<LinkIdx, T>;

/// A representation of the course layout of a composition, and how Monument understands
/// composition structure.  In this representation, a
/// layout is a set of [`Segment`]s, which are sequences of [`Row`]s combined with links to the
/// [`Segment`]s which can come after them.  Every useful composition structure (that I know of)
/// can be represented like this, but it is not efficient to use [`Layout`]s directly in the
/// composing loop.  Therefore, [`Engine`] compiles a `Layout` (along with extra info like desired
/// composition length, music requirements, etc.) into a node graph that can be efficiently
/// traversed.
#[derive(Debug, Clone)]
pub struct Layout {
    /// A list of blocks of [`Row`]s, from which the [`Segment`]s are taken (more precisely, each
    /// [`Segment`] corresponds to a subsequence of some block in `blocks`).  In most cases, this
    /// will be the plain course of the given method(s).
    pub blocks: Vec<Block>,
    /// The [`Link`]s by which segments of composition can be connected.  These are usually calls,
    /// but can also be the _absence_ of a call - note here that Monument will not implicitly add
    /// 'plain' links; they have to be explicitly added (and potentially named).
    ///
    /// Given a starting [`RowIdx`] of a course segment, Monument will extend it until the first
    /// [`Link`] which contains a matching course head [`Mask`].
    pub links: LinkVec<Link>,
    /// The [`RowIdx`]s and course heads where the composition can be started
    pub starts: Vec<StartOrEnd>,
    /// The [`RowIdx`]s and course heads where the composition can be finished.  If the composition
    /// starts and finishes at the same [`Row`], then `starts` and `ends` are likely to be equal
    /// (because every possible starting point is also an end point).  The only exceptions to this
    /// are cases where e.g. snap finishes are allowed but snap starts are not.
    pub ends: Vec<StartOrEnd>,
}

impl Layout {
    /// Create a new `Layout` for a single [`Method`].
    pub fn single_method(
        method: &Method,
        calls: &[self::Call],
        // The course head masks, along with the 'calling bell' for that course.  Allowing
        // different calling bells allows us to do things like keep using W,M,H during courses of
        // e.g. `1xxxxx0987`.
        ch_masks: Vec<(Mask, Bell)>,
        // Which sub-lead indices are considered valid starting or finishing points for the
        // composition.  If these are `None`, then any location is allowed
        allowed_start_indices: Option<&[usize]>,
        allowed_end_indices: Option<&[usize]>,
    ) -> Result<Self, single_method::Error> {
        single_method::single_method_layout(
            method,
            calls,
            ch_masks,
            allowed_start_indices,
            allowed_end_indices,
        )
    }

    /// Builds a [`Graph`] according to this `Layout`.
    pub fn to_graph(&self, music_types: &[MusicType], max_length: usize) -> Graph {
        Graph::from_layout(self, music_types, max_length)
    }

    /// Returns the [`Segment`], starting at a given [`NodeId`].  If this [`Segment`] would never
    /// terminate (because no [`Link`]s can be applied to it), then `None` is returned.
    pub(crate) fn get_segment(&self, id: &NodeId) -> Option<Segment> {
        let block_len = self.blocks[id.row_idx.block].len();
        let length_between = |from: usize, to: usize| (to + block_len - from) % block_len;

        // Figure out which links are going to finish this segment
        let mut outgoing_links = Vec::<(LinkIdx, NodeId)>::new();
        let mut shortest_length: Option<usize> = None;

        for (link_idx, link) in self.links.iter_enumerated() {
            // If this link doesn't come from the correct block, then it can't finish the segment
            if link.from.block != id.row_idx.block {
                continue;
            }
            // If this link's course head mask doesn't match the current course, then it can't be
            // called
            if !link.course_head_mask.matches(&id.course_head) {
                continue;
            }

            let length = length_between(id.row_idx.row, link.from.row);
            // Add one to the length, because `link.from` refers to the lead **end** not the lead
            // **head**
            let length = length + 1;

            let cmp_to_shortest_len = match shortest_length {
                Some(best_len) => length.cmp(&best_len),
                // If no lengths have been found, then all lengths are strictly better than no
                // length
                None => Ordering::Less,
            };

            // If this length is strictly better than the existing ones, then all the accumulated
            // links are no longer the best.  Additionally, this segment can no longer be an end
            // node if this link is taken before the end is reached.
            if cmp_to_shortest_len == Ordering::Less {
                outgoing_links.clear();
                shortest_length = Some(length);
            }
            // If this node is at least as good as the current best, then add this link to the list
            if cmp_to_shortest_len != Ordering::Greater {
                outgoing_links.push((link_idx, link.id_after(&id.course_head)));

                /* println!(
                    "{:>3} --[{}]-> {:>3} = {} ({:?})",
                    id.row_idx.row_idx,
                    link.debug_name,
                    link.from.row_idx,
                    length,
                    cmp_to_shortest_len
                ); */
            }
        }

        let mut end_label = None;
        // Determine whether or not this segment can end the composition before any links can be
        // taken
        for end in &self.ends {
            if end.ch_and_row_idx() == id.ch_and_row_idx() {
                let len = length_between(id.row_idx.row, end.row_idx.row);
                // Make a special case to disallow 0-length blocks which are both starts and ends.
                // This happens a lot because almost all compositions start and end at the same row
                // (i.e. rounds), and therefore the starts and ends will happen at the same
                // locations.  Therefore, each start node would generate a 0-length segment,
                // corresponding to a 0-length composition which immediately comes round.  This is
                // clearly not useful, so we explicitly prevent it here.
                if len == 0 && id.is_start {
                    continue;
                }

                let is_improvement = match shortest_length {
                    // If this node ends at the same location that a link could be taken, then the
                    // links take precedence (hence the strict inequality)
                    Some(l) => len < l,
                    // Any length is an improvement over an infinite length
                    None => true,
                };
                if is_improvement {
                    end_label = Some(end.label.to_owned());
                    shortest_length = Some(len);
                    outgoing_links.clear();
                }
            }
        }

        // Decide which of `self.starts` this node corresponds to (if it is a start)
        let start_idx = if id.is_start {
            let start_or_end = self
                .starts
                .iter()
                .find(|start| start.ch_and_row_idx() == id.ch_and_row_idx());
            // Sanity check that nodes marked `is_start` actually do correspond to a start node
            match start_or_end {
                Some(start) => Some(start.label.to_owned()),
                None => panic!("NodeId has `is_start`, but it doesn't come from `Layout::starts`"),
            }
        } else {
            None
        };

        // De-duplicate the links (removing pairs of links which link to the same node).  We do
        // already perform some de-duplication when building the Layout, but this deduplication is
        // also necessary in case we end up with two links that have different course head masks
        // that both match the current course.  For example, if we have `pB` matching `1xxxxx7890`
        // (generated by `xB`) and `pB` matching `1234567xx0` (generated by potentially calling
        // BFI) then these would not be de-duplicated earlier but both match the plain course (CH
        // `1234567890`).
        let mut deduped_links = Vec::<(LinkIdx, NodeId)>::with_capacity(outgoing_links.len());
        for (link_idx, resulting_id) in outgoing_links {
            let link = &self.links[link_idx];
            // Only push links if they are different to all nodes pushed so far to `deduped_links`
            if deduped_links
                .iter()
                .all(|(idx2, _id)| !link.eq_without_name_or_ch_mask(&self.links[*idx2]))
            {
                deduped_links.push((link_idx, resulting_id));
            }
        }

        // If some way of ending this segment was found (i.e. a Link or an end-point), then build a
        // new Some(Segment), otherwise bubble the `None` value
        shortest_length.map(|length| Segment {
            links: deduped_links,
            length,
            node_id: id.clone(),
            start_label: start_idx,
            end_label,
        })
    }

    /// Gets the [`RowIdx`] of the last row within a [`RowRange`] (or `None` if that range has size
    /// 0).
    pub(crate) fn last_row_idx(&self, row_range: RowRange) -> Option<RowIdx> {
        (row_range.length > 0).then(|| {
            let block_len = self.blocks[row_range.start.block].len();
            RowIdx::new(
                row_range.start.block,
                // The subtraction here cannot overflow, because this code only executes when
                // `row_range.length > 0`
                (row_range.start.row + row_range.length - 1) % block_len,
            )
        })
    }

    /// Return the [`Row`]s covered by a given range
    pub(crate) fn untransposed_rows(
        &self,
        row_idx: RowIdx,
        length: usize,
    ) -> impl Iterator<Item = &'_ Row> {
        self.blocks[row_idx.block]
            .rows()
            .cycle()
            .skip(row_idx.row)
            .take(length)
    }
}

/// A link between two segments of a course
#[derive(Debug, Clone)]
pub struct Link {
    /// Which [`Row`] in the [`Layout`] this `Link` starts from.  This is a half-open bound - for
    /// example, if this `Link` represents a call over the lead end then this index refers to the
    /// lead **head**, not the lead **end**.
    pub from: RowIdx,
    /// Which [`Row`] the composition will be at after this `Link` is taken
    pub to: RowIdx,

    /// A [`Mask`] which determines which course heads this `Link` can be applied to
    pub course_head_mask: Mask,
    /// The transposition of the course head taken when this is applied
    pub course_head_transposition: RowBuf,

    /// The name of this `Link`, used in debugging
    pub debug_name: String,
    /// The name of this `Link` used when generating human-friendly call strings
    pub display_name: String,
}

impl Link {
    /// Gets the [`NodeId`] of the node that would appear after this [`Link`] is applied to a given
    /// course.
    fn id_after(&self, course_head: &Row) -> NodeId {
        // Sanity check that this link could actually be applied in this location.
        assert!(self.course_head_mask.matches(course_head));
        NodeId::new(
            course_head * self.course_head_transposition.as_row(),
            self.to,
            // Nodes reached by taking a link can't be start nodes
            false,
        )
    }

    /// Returns `true` if `self` and `other` are equal (but ignoring the name and CH masks)
    pub(crate) fn eq_without_name_or_ch_mask(&self, other: &Self) -> bool {
        self.from == other.from
            && self.to == other.to
            && self.course_head_transposition == other.course_head_transposition
    }
}

/// A point where the composition can start or stop.  This is usually the location of rounds within
/// the composition graph.
#[derive(Debug, Clone)]
pub struct StartOrEnd {
    pub course_head: RowBuf,
    pub row_idx: RowIdx,
    pub label: String,
}

impl StartOrEnd {
    fn ch_and_row_idx(&self) -> (&Row, RowIdx) {
        (&self.course_head, self.row_idx)
    }
}

/// The unique index of a [`Row`] within a [`Layout`].  This is essentially a `(block_idx,
/// row_idx)` pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RowIdx {
    pub block: usize,
    pub row: usize,
}

impl RowIdx {
    pub fn new(block_idx: usize, row_idx: usize) -> Self {
        Self {
            block: block_idx,
            row: row_idx,
        }
    }
}

/// The unique identifier for a single node (i.e. an instantiated course segment) in the
/// composition.  This node is assumed to end at the closest [`Link`] where the course head matches
/// one of the supplied [course head masks](Link::course_head_masks).
#[derive(Clone, Eq, PartialEq, Hash)]
pub struct NodeId {
    pub course_head: Arc<Row>, // `Arc` is used to make cloning cheaper
    pub row_idx: RowIdx,
    // Start nodes have to be treated separately in the case where the rounds can appear as the
    // first [`Row`] of a segment.  In this case, the start segment is full-length whereas any
    // non-start segments become 0-length end segments (because the composition comes round
    // instantly).
    pub is_start: bool,
}

impl NodeId {
    pub fn new(course_head: RowBuf, row_idx: RowIdx, is_start: bool) -> Self {
        Self {
            course_head: course_head.to_arc(),
            row_idx,
            is_start,
        }
    }

    pub fn ch_and_row_idx(&self) -> (&Row, RowIdx) {
        (&self.course_head, self.row_idx)
    }
}

impl Debug for NodeId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "NodeId({})", self)
    }
}

impl Display for NodeId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{},{}:{}{}",
            self.course_head,
            self.row_idx.block,
            self.row_idx.row,
            if self.is_start { ",is_start" } else { "" }
        )
    }
}

/// A range of [`Row`]s covered by a [`Segment`]
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct RowRange {
    pub start: RowIdx,
    pub length: usize,
}

impl RowRange {
    pub fn new(start: RowIdx, length: usize) -> Self {
        Self { start, length }
    }
}

/// A section of a composition with no internal links, uniquely determined by a [`NodeId`] (which
/// specifies the first row of the [`Segment`]).
#[derive(Debug, Clone)]
pub(crate) struct Segment {
    pub(crate) node_id: NodeId,
    pub(crate) length: usize,
    pub(crate) links: Vec<(LinkIdx, NodeId)>,

    /// If this `Segment` is a start point, then this is `Some(s)` where `s` should be inserted at
    /// the start of the composition string (e.g. `""` for normal starts and `"<"` for snap
    /// starts).  This `Segment` is a 'start' segment if and only if this is `Some(_)`.
    pub(crate) start_label: Option<String>,
    /// If this `Segment` is a end point, then this is `Some(s)` where `s` should be inserted at
    /// the end of the composition string (e.g. `""` for normal starts and `">"` for snap
    /// starts).  This `Segment` is an 'end' segment if and only if this is `Some(_)`.
    pub(crate) end_label: Option<String>,
}

impl Segment {
    pub(crate) fn is_end(&self) -> bool {
        self.end_label.is_some()
    }

    /// Returns the [`Row`]s covered this [`Segment`] **of the plain course**
    pub(crate) fn untransposed_rows<'l>(
        &self,
        layout: &'l Layout,
    ) -> impl Iterator<Item = &'l Row> {
        layout.untransposed_rows(self.node_id.row_idx, self.length)
    }
}

/// A way of labelling the calls in a set of courses.
#[derive(Debug, Clone)]
pub struct CourseHeadMask {
    mask: Mask,
    /// The bell who's position determines the names of the [`Call`]s in this course.
    ///
    /// **Invariant**: `mask` must specify a location for this [`Bell`].  This means that every
    /// call at a given position through a course will be given the same name.
    calling_bell: Bell,
}

impl CourseHeadMask {
    /// Converts a [`Mask`] and a `calling_bell` into a set of [`CourseHeadMask`]s which, between
    /// them, match the same rows as the source [`Mask`] but all **explicitly** specify a position
    /// for the `calling_bell`.  This way, if two calls have the same position within a course,
    /// they must be given the same calling position (this makes graph expansion much simpler).
    ///
    /// An example where this expansion is needed is if the tenor (the `8`) is used as a calling
    /// bell for the course mask `12345xxx`.  This generates a situation where the same call is
    /// given different names depending on the exact course head used (e.g. a call at `123458xx`
    /// would be called a `M`, whereas a call at `12345xx8` would be called `H`).
    pub(crate) fn new(mask: Mask, calling_bell: Bell) -> Vec<Self> {
        if mask.place_of(calling_bell).is_some() {
            return vec![Self { mask, calling_bell }];
        } else {
            mask.unspecified_places()
                .map(|pl| {
                    let mut new_mask = mask.to_owned();
                    // Unwrap is safe because the calling bell can't already be in the mask
                    new_mask.set_bell(calling_bell, pl).unwrap();
                    Self {
                        mask: new_mask,
                        calling_bell,
                    }
                })
                .collect_vec()
        }
    }

    pub fn mask(&self) -> &Mask {
        &self.mask
    }

    pub fn calling_bell(&self) -> Bell {
        self.calling_bell
    }
}

///////////
// CALLS //
///////////

/// The specification for a call that can be used in a composition
#[derive(Debug, Clone)]
pub struct Call {
    display_symbol: String,
    debug_symbol: String,
    lead_location: String,
    place_not: PlaceNot,
    calling_positions: Vec<String>,
}

impl Call {
    pub fn new(
        display_symbol: String,
        debug_symbol: String,
        lead_location: String,
        place_not: PlaceNot,
        calling_positions: Option<Vec<String>>,
    ) -> Self {
        Self {
            display_symbol,
            debug_symbol,
            lead_location,
            calling_positions: calling_positions
                .unwrap_or_else(|| default_calling_positions(&place_not)),
            place_not,
        }
    }

    ////////////////////////
    // DEFAULT CALL TYPES //
    ////////////////////////

    /// Generates `14` bob and `1234` single, both at the lead end (i.e. label `"LE"`).  Returns
    /// `None` for any [`Stage`] smaller than [`Stage::MINIMUS`].
    pub fn near_calls(stage: Stage) -> Option<Vec<Self>> {
        let bob = Self::lead_end_bob(PlaceNot::parse("14", stage).ok()?);
        let single = Self::lead_end_bob(PlaceNot::parse("1234", stage).ok()?);
        Some(vec![bob, single])
    }

    /// Generates `1(n-2)` bob and `1(n-2)(n-1)n` single, both at the lead end (i.e. label `"LE"`).
    /// Returns `None` for any [`Stage`] smaller than [`Stage::MINIMUS`].
    pub fn far_calls(stage: Stage) -> Option<Vec<Self>> {
        if stage < Stage::MINIMUS {
            return None;
        }

        let n = stage.num_bells();
        // Unsafety and unwrapping is OK because, in both cases, the places are sorted and within
        // the stage (because we early return when `n < 4`).
        let bob_notation = unsafe { PlaceNot::from_sorted_slice(&[1, n - 2], stage).unwrap() };
        let single_notation =
            unsafe { PlaceNot::from_sorted_slice(&[1, n - 2, n - 1, n], stage).unwrap() };

        let bob = Self::lead_end_bob(bob_notation);
        let single = Self::lead_end_bob(single_notation);
        Some(vec![bob, single])
    }

    /// Create a bob which replaces the lead end with a given [`PlaceNot`]
    pub fn lead_end_bob(place_not: PlaceNot) -> Self {
        Self::new(
            String::new(),
            "-".to_owned(),
            bellframe::method::LABEL_LEAD_END.to_owned(),
            place_not,
            None,
        )
    }

    /// Create a bob which replaces the lead end with a given [`PlaceNot`]
    pub fn lead_end_single(place_not: PlaceNot) -> Self {
        Self::new(
            "s".to_owned(),
            "s".to_owned(),
            bellframe::method::LABEL_LEAD_END.to_owned(),
            place_not,
            None,
        )
    }
}

fn default_calling_positions(place_not: &PlaceNot) -> Vec<String> {
    let named_positions = "LIBFVXSEN";

    // Generate calling positions that aren't M, W or H
    let mut positions =
        // Start off with the single-char position names
        named_positions
        .chars()
        .map(|c| c.to_string())
        // Extending forever with numbers
        .chain((named_positions.len()..).map(|i| (i + 1).to_string()))
        // But we consume one value per place in the Stage
        .take(place_not.stage().num_bells())
        .collect_vec();

    /// A cheeky macro which generates the code to perform an in-place replacement of a calling
    /// position at a given (0-indexed) place
    macro_rules! replace_pos {
        ($idx: expr, $new_val: expr) => {
            if let Some(v) = positions.get_mut($idx) {
                v.clear();
                v.push($new_val);
            }
        };
    }

    // Edge case: if 2nds are made in `place_not`, then I/B are replaced with B/T.  Note that
    // places are 0-indexed
    if place_not.contains(1) {
        replace_pos!(1, 'B');
        replace_pos!(2, 'T');
    }

    /// A cheeky macro which generates the code to perform an in-place replacement of a calling
    /// position at a place indexed from the end of the stage (so 0 is the highest place)
    macro_rules! replace_mwh {
        ($ind: expr, $new_val: expr) => {
            if let Some(place) = place_not.stage().num_bells().checked_sub(1 + $ind) {
                if place >= 4 {
                    if let Some(v) = positions.get_mut(place) {
                        v.clear();
                        v.push($new_val);
                    }
                }
            }
        };
    }

    // Add MWH (M and W are swapped round for odd stages)
    if place_not.stage().is_even() {
        replace_mwh!(2, 'M');
        replace_mwh!(1, 'W');
        replace_mwh!(0, 'H');
    } else {
        replace_mwh!(2, 'W');
        replace_mwh!(1, 'M');
        replace_mwh!(0, 'H');
    }

    positions
}

#[cfg(test)]
mod tests {
    use bellframe::{PlaceNot, Stage};
    use itertools::Itertools;

    fn char_vec(string: &str) -> Vec<String> {
        string.chars().map(|c| c.to_string()).collect_vec()
    }

    #[test]
    fn default_calling_positions() {
        #[rustfmt::skip]
        let cases = &[
            ("145", Stage::DOUBLES, char_vec("LIBFH")),
            ("125", Stage::DOUBLES, char_vec("LBTFH")),
            ("1", Stage::DOUBLES, char_vec("LIBFH")),

            ("14", Stage::MINOR, char_vec("LIBFWH")),
            ("1234", Stage::MINOR, char_vec("LBTFWH")),
            ("1456", Stage::MINOR, char_vec("LIBFWH")),

            ("147", Stage::TRIPLES, char_vec("LIBFWMH")),
            ("12347", Stage::TRIPLES, char_vec("LBTFWMH")),

            ("14", Stage::MAJOR, char_vec("LIBFVMWH")),
            ("1234", Stage::MAJOR, char_vec("LBTFVMWH")),
            ("16", Stage::MAJOR, char_vec("LIBFVMWH")),
            ("1678", Stage::MAJOR, char_vec("LIBFVMWH")),
            ("1256", Stage::MAJOR, char_vec("LBTFVMWH")),
            ("123456", Stage::MAJOR, char_vec("LBTFVMWH")),

            ("14", Stage::ROYAL, char_vec("LIBFVXSMWH")),
            ("16", Stage::ROYAL, char_vec("LIBFVXSMWH")),
            ("18", Stage::ROYAL, char_vec("LIBFVXSMWH")),
            ("1890", Stage::ROYAL, char_vec("LIBFVXSMWH")),

            ("14", Stage::MAXIMUS, char_vec("LIBFVXSENMWH")),
            ("1234", Stage::MAXIMUS, char_vec("LBTFVXSENMWH")),
        ];

        for (pn_str, stage, exp_positions) in cases {
            let positions =
                super::default_calling_positions(&PlaceNot::parse(pn_str, *stage).unwrap());
            assert_eq!(positions, *exp_positions);
        }
    }
}