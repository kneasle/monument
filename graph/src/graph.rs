//! A mutable graph of nodes.  Compositions are represented as paths through this node graph.

use std::{
    cmp::{Ordering, Reverse},
    collections::{BinaryHeap, HashMap, HashSet},
};

use bellframe::{Row, RowBuf};
use itertools::Itertools;
use log::log;
use monument_layout::{
    node_range::{End, NodeRange, PerPartLength, RangeEnd, RangeFactory, TotalLength},
    Layout, LinkIdx, NodeId, Rotation, RowRange, StandardNodeId, StartIdx,
};
use monument_utils::{FrontierItem, RowCounts};

use crate::{
    falseness::FalsenessTable,
    music::{Breakdown, MusicType, Score},
    optimise::Pass,
    Data,
};

/// The number of rows required to get from a point in the graph to a start/end.
type Distance = usize;

/// A 'prototype' node graph that is (relatively) inefficient to traverse but easy to modify.  This
/// is usually used to build and optimise the node graph before being converted into an efficient
/// graph representation for use in tree search.
#[derive(Debug, Clone)]
pub struct Graph {
    // NOTE: References between nodes don't have to be valid (i.e. they can point to a [`Node`]
    // that isn't actually in the graph - in this case they will be ignored or discarded during the
    // optimisation process).
    nodes: HashMap<NodeId, Node>,
    /// **Invariant**: If `start_nodes` points to a node, it **must** be a start node (i.e. not
    /// have any predecessors, and have `start_label` set)
    start_nodes: Vec<(NodeId, StartIdx, Rotation)>,
    /// **Invariant**: If `start_nodes` points to a node, it **must** be a end node (i.e. not have
    /// any successors, and have `end_nodes` set)
    end_nodes: Vec<(NodeId, End)>,
    /// The number of different parts
    num_parts: Rotation,
}

/// A `Node` in a node [`Graph`].  This is an indivisible chunk of ringing which cannot be split up
/// by calls or splices.
#[derive(Debug, Clone)]
pub struct Node {
    /// If this `Node` is a 'start' (i.e. it can be the first node in a composition), then this is
    /// `Some(label)` where `label` should be appended to the front of the human-friendly
    /// composition string.
    is_start: bool,
    /// If this `Node` is an 'end' (i.e. adding it will complete a composition), then this is
    /// `Some(label)` where `label` should be appended to the human-friendly composition string.
    end: Option<End>,
    /// The string that should be added when this node is generated
    label: String,

    successors: Vec<Link>,
    predecessors: Vec<Link>,

    /// The nodes which share rows with `self`, including `self` (because all nodes are false
    /// against themselves).  Optimisation passes probably shouldn't mess with falseness.
    false_nodes: Vec<StandardNodeId>,

    /// The number of rows in the range covered by this node (i.e. its length in one part of the
    /// composition)
    per_part_length: PerPartLength,
    /// The number of rows that this this node adds to the composition (its total length across all
    /// parts).  Optimisation passes can't change this
    total_length: TotalLength,
    /// The number of rows of each method generated by this node
    method_counts: RowCounts,
    /// The music generated by this node in the composition.  Optimisation passes can't change this
    music: Breakdown,

    /* MUTABLE STATE FOR OPTIMISATION PASSES */
    /// Does this node need to be included in every composition in this search?
    pub required: bool,
    /// A lower bound on the number of rows required to go from any rounds to the first row of
    /// `self`
    pub lb_distance_from_rounds: Distance,
    /// A lower bound on the number of rows required to go from the first row **after** `self` to
    /// rounds.
    pub lb_distance_to_rounds: Distance,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct Link {
    pub id: NodeId,
    /// Indexes into `Layout::links`
    pub source_idx: LinkIdx,
    pub rotation: Rotation,
}

impl Link {
    pub fn new(id: NodeId, source_idx: LinkIdx, rotation: Rotation) -> Self {
        Self {
            id,
            source_idx,
            rotation,
        }
    }
}

// ------------------------------------------------------------------------------------------

impl Graph {
    //! Optimisation

    /// Repeatedly apply a sequence of [`Pass`]es until the graph stops getting smaller, or 20
    /// iterations are made.  Use [`Graph::optimise_with_iter_limit`] to set a custom iteration limit.
    pub fn optimise(&mut self, passes: &mut [Pass], data: &Data) {
        self.optimise_with_iter_limit(passes, data, 20);
    }

    /// Repeatedly apply a sequence of [`Pass`]es until the graph either becomes static, or `limit`
    /// many iterations are performed.
    pub fn optimise_with_iter_limit(&mut self, passes: &mut [Pass], data: &Data, limit: usize) {
        let mut last_size = Size::from(&*self);

        for _ in 0..limit {
            self.run_passes(passes, data);

            let new_size = Size::from(&*self);
            // Stop optimising if the optimisations don't make the graph strictly smaller.  If
            // they make some parts smaller but others larger, then keep optimising.
            match new_size.partial_cmp(&last_size) {
                Some(Ordering::Equal | Ordering::Greater) => return,
                Some(Ordering::Less) | None => {}
            }
            last_size = new_size;
        }
    }

    /// Run a sequence of [`Pass`]es on `self`
    pub fn run_passes(&mut self, passes: &mut [Pass], data: &Data) {
        for p in &mut *passes {
            p.run(self, data);
        }
    }

    /// For each start node in `self`, creates a copy of `self` with _only_ that start node.  This
    /// partitions the set of generated compositions across these `Graph`s, but allows for better
    /// optimisations because more is known about each `Graph`.
    pub fn split_by_start_node(&self) -> Vec<Graph> {
        self.start_nodes
            .iter()
            .cloned()
            .map(|start_id| {
                let mut new_self = self.clone();
                new_self.start_nodes = vec![start_id];
                new_self
            })
            .collect_vec()
    }

    pub fn num_parts(&self) -> Rotation {
        self.num_parts
    }
}

// ------------------------------------------------------------------------------------------

impl Graph {
    //! Helpers for optimisation passes

    /// Removes all nodes for whom `pred` returns `false`
    pub fn retain_nodes(&mut self, pred: impl FnMut(&NodeId, &mut Node) -> bool) {
        self.nodes.retain(pred);
    }

    /// Remove elements from [`Self::start_nodes`] for which a predicate returns `false`.
    pub fn retain_start_nodes(&mut self, pred: impl FnMut(&(NodeId, StartIdx, Rotation)) -> bool) {
        self.start_nodes.retain(pred);
    }

    /// Remove elements from [`Self::end_nodes`] for which a predicate returns `false`.
    pub fn retain_end_nodes(&mut self, pred: impl FnMut(&(NodeId, End)) -> bool) {
        self.end_nodes.retain(pred);
    }
}

impl Node {
    //! Helpers for optimisation passes

    /// A lower bound on the length of a composition which passes through this node.
    pub fn min_comp_length(&self) -> usize {
        self.lb_distance_from_rounds + self.total_length.0 + self.lb_distance_to_rounds
    }
}

/// A measure of the `Size` of a [`Graph`].  Used to detect when further optimisations aren't
/// useful.
#[derive(Debug, PartialEq, Clone, Copy)]
struct Size {
    num_nodes: usize,
    num_links: usize,
    num_starts: usize,
    num_ends: usize,
}

impl From<&Graph> for Size {
    fn from(g: &Graph) -> Self {
        Self {
            num_nodes: g.nodes.len(),
            // This assumes that every successor link also corresponds to a predecessor link
            num_links: g.nodes().map(|(_id, node)| node.successors.len()).sum(),
            num_starts: g.start_nodes.len(),
            num_ends: g.end_nodes.len(),
        }
    }
}

impl PartialOrd for Size {
    // TODO: Make this into a macro?
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        let cmp_nodes = self.num_nodes.cmp(&other.num_nodes);
        let cmp_links = self.num_links.cmp(&other.num_links);
        let cmp_starts = self.num_starts.cmp(&other.num_starts);
        let cmp_ends = self.num_ends.cmp(&other.num_ends);

        let all_comparisons = [cmp_nodes, cmp_links, cmp_starts, cmp_ends];

        let are_any_less = all_comparisons
            .iter()
            .any(|cmp| matches!(cmp, Ordering::Less));
        let are_any_greater = all_comparisons
            .iter()
            .any(|cmp| matches!(cmp, Ordering::Greater));

        match (are_any_less, are_any_greater) {
            (true, false) => Some(Ordering::Less), // If nothing got larger, then the size is smaller
            (false, true) => Some(Ordering::Greater), // If nothing got smaller, then the size is larger
            (false, false) => Some(Ordering::Equal),  // No < or > means all components are equal
            (true, true) => None, // If some are smaller & some are greater then these are incomparable
        }
    }
}

// ------------------------------------------------------------------------------------------

impl Graph {
    //! Getters & Iterators

    // Getters

    pub fn get_node<'graph>(&'graph self, id: &NodeId) -> Option<&'graph Node> {
        self.nodes.get(id)
    }

    pub fn get_node_mut<'graph>(&'graph mut self, id: &NodeId) -> Option<&'graph mut Node> {
        self.nodes.get_mut(id)
    }

    pub fn start_nodes(&self) -> &[(NodeId, StartIdx, Rotation)] {
        &self.start_nodes
    }

    pub fn end_nodes(&self) -> &[(NodeId, End)] {
        &self.end_nodes
    }

    pub fn node_map(&self) -> &HashMap<NodeId, Node> {
        &self.nodes
    }

    pub fn get_start(&self, idx: usize) -> Option<(&Node, StartIdx, Rotation)> {
        let (start_node_id, start_idx, rotation) = self.start_nodes.get(idx)?;
        let start_node = self.nodes.get(start_node_id)?;
        assert!(start_node.is_start);
        Some((start_node, *start_idx, *rotation))
    }

    // Iterators

    /// An [`Iterator`] over the [`NodeId`] of every [`Node`] in this `Graph`
    pub fn ids(&self) -> impl Iterator<Item = &NodeId> {
        self.nodes.keys()
    }

    /// An [`Iterator`] over every [`Node`] in this `Graph` (including its [`NodeId`])
    pub fn nodes(&self) -> impl Iterator<Item = (&NodeId, &Node)> {
        self.nodes.iter()
    }

    /// An [`Iterator`] over every [`Node`] in this `Graph`, without its [`NodeId`].
    pub fn just_nodes(&self) -> impl Iterator<Item = &Node> {
        self.nodes.values()
    }

    /// A mutable [`Iterator`] over the [`NodeId`] of every [`Node`] in this `Graph`
    pub fn nodes_mut(&mut self) -> impl Iterator<Item = (&NodeId, &mut Node)> {
        self.nodes.iter_mut()
    }
}

impl Node {
    //! Getters & Iterators

    pub fn length(&self) -> usize {
        self.total_length.0
    }

    pub fn method_counts(&self) -> &RowCounts {
        &self.method_counts
    }

    pub fn score(&self) -> Score {
        self.music.total
    }

    pub fn label(&self) -> &str {
        self.label.as_str()
    }

    // STARTS/ENDS //

    pub fn is_start(&self) -> bool {
        self.is_start
    }

    pub fn end(&self) -> Option<End> {
        self.end
    }

    pub fn is_end(&self) -> bool {
        self.end.is_some()
    }

    // CROSS-NODE REFERENCES //

    pub fn successors(&self) -> &[Link] {
        self.successors.as_slice()
    }

    pub fn successors_mut(&mut self) -> &mut Vec<Link> {
        &mut self.successors
    }

    pub fn predecessors(&self) -> &[Link] {
        self.predecessors.as_slice()
    }

    pub fn predecessors_mut(&mut self) -> &mut Vec<Link> {
        &mut self.predecessors
    }

    pub fn false_nodes(&self) -> &[StandardNodeId] {
        self.false_nodes.as_slice()
    }

    pub fn false_nodes_mut(&mut self) -> &mut Vec<StandardNodeId> {
        &mut self.false_nodes
    }
}

////////////////////////////////
// LAYOUT -> GRAPH CONVERSION //
////////////////////////////////

impl Graph {
    /// Generate a graph of all nodes which are reachable within a given length constraint.
    pub fn from_layout(
        layout: &Layout,
        music_types: &[MusicType],
        max_length: usize,
        part_head: &Row,
    ) -> Self {
        // Build the shape of the graph using Dijkstra's algorithm
        let (expanded_node_ranges, start_nodes, end_nodes, ch_equiv_map, part_heads) =
            build_graph(layout, max_length, part_head);
        let num_parts = part_heads.len() as Rotation;

        // Convert each `expanded_node_range` into a full `Node`, albeit without
        // predecessor/falseness references
        let mut nodes: HashMap<NodeId, Node> = expanded_node_ranges
            .iter()
            .map(|(node_id, (node_range, distance))| {
                assert_eq!(node_id, &node_range.node_id);
                let new_node = build_node(node_range, *distance, layout, music_types, &part_heads);
                (node_id.clone(), new_node)
            })
            .collect();

        let plural = |count: usize, singular: &str| -> String {
            let extension = if count == 1 { "" } else { "s" };
            format!("{} {}{}", count, singular, extension)
        };
        log::info!(
            "Graph has {}, with {} and {}.",
            plural(nodes.len(), "node"),
            plural(start_nodes.len(), "start"),
            plural(end_nodes.len(), "end"),
        );

        compute_falseness(&mut nodes, layout, &ch_equiv_map);

        // Add predecessor references (every node is a predecessor to all of its successors)
        log::debug!("Setting predecessor links");
        for (id, _dist) in expanded_node_ranges {
            for succ_link in nodes.get(&id).unwrap().successors.clone() {
                if let Some(node) = nodes.get_mut(&succ_link.id) {
                    assert!(succ_link.rotation < num_parts);
                    node.predecessors.push(Link {
                        id: id.clone(),
                        source_idx: succ_link.source_idx,
                        // Passing backwards over a link gives it the opposite rotation to
                        // traversing forward
                        rotation: num_parts - succ_link.rotation,
                    });
                }
            }
        }

        Self {
            nodes,
            start_nodes,
            end_nodes,
            num_parts,
        }
    }
}

fn compute_falseness(
    nodes: &mut HashMap<NodeId, Node>,
    layout: &Layout,
    ch_equiv_map: &HashMap<RowBuf, (RowBuf, Rotation)>,
) {
    log::debug!("Building falseness table");
    let node_ids_and_lengths = nodes
        .iter()
        .map(|(id, node)| (id.clone(), node.per_part_length))
        .collect::<HashSet<_>>();
    let falseness_table = FalsenessTable::from_layout(layout, &node_ids_and_lengths);
    log::trace!("Falseness table: {:#?}", falseness_table);
    log::debug!("Setting falseness links");
    for (id, node) in nodes.iter_mut() {
        // Only compute falseness for standard IDs
        if let NodeId::Standard(std_id) = id {
            let range = RowRange {
                start: std_id.row_idx,
                len: node.per_part_length,
            };
            node.false_nodes.clear();
            for (false_range, false_ch_transposition) in falseness_table.false_course_heads(range) {
                let false_ch = std_id.course_head.as_ref() * false_ch_transposition;
                if let Some((false_equiv_ch, _rotation)) = ch_equiv_map.get(&false_ch) {
                    for is_start in [true, false] {
                        let false_id = StandardNodeId::new(
                            false_equiv_ch.clone(),
                            false_range.start,
                            is_start,
                        );
                        let false_id_and_len =
                            (NodeId::Standard(false_id.clone()), false_range.len);
                        if node_ids_and_lengths.contains(&false_id_and_len) {
                            // If the node at `false_id` is in the graph, then it's false against
                            // `node`
                            node.false_nodes.push(false_id);
                        }
                    }
                }
            }
        }
    }
}

/// Use Dijkstra's algorithm to determine the overall structure of the graph, without computing the
/// [`Node`]s themselves.
fn build_graph(
    layout: &Layout,
    max_length: usize,
    part_head: &Row,
) -> (
    HashMap<NodeId, (NodeRange, Distance)>,
    Vec<(NodeId, StartIdx, Rotation)>,
    Vec<(NodeId, End)>,
    HashMap<RowBuf, (RowBuf, Rotation)>,
    Vec<RowBuf>,
) {
    let mut range_factory = RangeFactory::new(layout, part_head);

    let start_nodes = range_factory.start_ids();
    let mut end_nodes = Vec::<(NodeId, End)>::new();

    // The set of nodes which have already been expanded
    let mut expanded_nodes: HashMap<NodeId, (NodeRange, Distance)> = HashMap::new();

    // Initialise the frontier with the start nodes, all with distance 0
    let mut frontier: BinaryHeap<Reverse<FrontierItem<NodeId>>> = BinaryHeap::new();
    frontier.extend(
        start_nodes
            .iter()
            .cloned()
            .map(|(id, _, _)| FrontierItem::new(id))
            .map(Reverse),
    );

    while let Some(Reverse(FrontierItem {
        item: node_id,
        distance,
    })) = frontier.pop()
    {
        // Don't expand nodes multiple times (Dijkstra's algorithm makes sure that the first time
        // it is expanded will be have the shortest distance)
        if expanded_nodes.get(&node_id).is_some() {
            continue;
        }
        // If the node hasn't been expanded yet, then add its reachable nodes to the frontier
        let node_range = range_factory
            .gen_range(&node_id)
            .expect("Infinite segment found");
        // If the shortest composition including this node is longer the length limit, then don't
        // include it in the node graph
        let new_dist = distance + node_range.total_length.0;
        if new_dist > max_length {
            continue;
        }
        match &node_range.range_end {
            RangeEnd::End(end) => end_nodes.push((node_id.clone(), *end)),
            RangeEnd::NotEnd(succ_links) => {
                // Expand the node by adding its successors to the frontier
                for (_, id_after_link, _) in succ_links {
                    // Add the new node to the frontier
                    frontier.push(Reverse(FrontierItem {
                        item: id_after_link.to_owned(),
                        distance: new_dist,
                    }));
                }
            }
        }
        // Mark this node as expanded
        expanded_nodes.insert(node_id, (node_range, distance));
    }

    // Once Dijkstra's has finished, consume the `RangeFactory` and return the values needed to
    // complete the graph
    let (ch_equiv_map, part_heads) = range_factory.finish();
    (
        expanded_nodes,
        start_nodes,
        end_nodes,
        ch_equiv_map,
        part_heads,
    )
}

fn build_node(
    node_range: &NodeRange,
    distance: usize,
    layout: &Layout,
    music_types: &[MusicType],
    part_heads: &[RowBuf],
) -> Node {
    // Add up music from each part
    let mut music = Breakdown::zero(music_types.len());
    for ph in part_heads {
        if let Some(source_ch) = node_range.node_id.course_head() {
            let ch = ph * source_ch;
            music += &Breakdown::from_rows(node_range.untransposed_rows(layout), &ch, music_types);
        }
    }

    Node {
        per_part_length: node_range.per_part_length,
        total_length: node_range.total_length,

        method_counts: node_range.method_counts.clone(),
        music,

        is_start: node_range.node_id.is_start(),
        end: node_range.end(),
        label: node_range.label.clone(),

        required: false,
        lb_distance_from_rounds: distance,
        // Distances to rounds are computed later, but the distance is an lower bound,
        // so we can set it to 0 without breaking any invariants.
        lb_distance_to_rounds: 0,

        successors: node_range
            .links()
            .iter()
            .cloned()
            .map(|(idx, id, rotation)| Link::new(id, idx, rotation))
            .collect_vec(),

        // These are populated in separate passes once all the `Node`s have been created
        false_nodes: Vec::new(),
        predecessors: Vec::new(),
    }
}
