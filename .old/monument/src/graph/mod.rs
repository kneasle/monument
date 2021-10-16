//! A 'prototype' node graph that is inefficient to traverse but easy to modify.  This is generated
//! from the [`Layout`], then optimised and finally converted into in-memory node graphs for each
//! thread.

use std::{
    cmp::{Ordering, Reverse},
    collections::{BinaryHeap, HashMap, HashSet, VecDeque},
};

use itertools::Itertools;

use crate::{
    compose::{CompPrefix, QueueElem},
    music::Breakdown,
    score::Score,
    spec::{
        layout::{Layout, NodeId, Segment},
        Config, SuccSortStrat,
    },
    MusicType,
};

use falseness::FalsenessTable;

/// Fast falseness computations used whilst generating node graphs
mod falseness;

/// A 'prototype' node graph that is inefficient to traverse but easy to modify.  This is used to
/// build and optimise the node graph before being converted into an efficient [`Graph`] structure
/// for use in tree search.
#[derive(Debug, Clone)]
pub struct Graph {
    // NOTE: References between nodes don't have to be valid (i.e. they can point to a
    // [`ProtoNode`] that isn't actually in the graph - in this case they will be ignored or
    // discarded during the optimisation process).
    pub(crate) nodes: HashMap<NodeId, Node>,
    start_nodes: Vec<NodeId>,
}

impl Graph {
    /// Generate and optimise a graph from a [`Layout`]
    pub fn from_layout(
        layout: &Layout,
        music_types: &[MusicType],
        max_length: usize,
        config: &Config,
    ) -> Self {
        let mut graph = Self::reachable_graph(layout, music_types, max_length);

        let mut last_num_nodes = graph.nodes.len();

        // Repeatedly optimise the graph until it stops getting smaller
        loop {
            // Recompute distances and remove any nodes which can't be reached in a short enough
            // composition
            graph.recompute_distances_from_rounds();
            graph.recompute_distances_to_rounds();
            graph.remove_nodes_by_distance(max_length);

            // Other optimisation ideas:
            // - Remove nodes which are false against any required nodes
            // - Merge chunks of nodes which can only be rung together (i.e. each pair has
            //   precisely one successor and predecessor).
            // - Remove successor links which connect two nodes which are false (e.g. calling
            //   singles in Kent).  These don't need to exist since they can never be explored.

            // If the graph has not got smaller, then further optimisation is not useful so we can
            // stop optimising
            let num_nodes = graph.nodes.len();
            // println!("Graph reduction: {} -> {}", last_num_nodes, num_nodes);
            if num_nodes == last_num_nodes {
                break;
            }
            last_num_nodes = num_nodes;
        }

        // Perform final optimisations and return
        graph.prune_references();
        graph.sort_successors(config);
        graph
    }

    /// Generate a graph of all nodes which are reachable within a given length constraint.
    fn reachable_graph(layout: &Layout, music_types: &[MusicType], max_length: usize) -> Self {
        // The set of reachable nodes and whether or not they are a start node (each mapping to a
        // distance from rounds)
        let mut expanded_nodes: HashMap<NodeId, (Segment, usize)> = HashMap::new();

        // Unexplored nodes, ordered by distance from rounds (i.e. the minimum number of rows required
        // to reach them from rounds)
        let mut frontier: BinaryHeap<Reverse<FrontierNode>> = BinaryHeap::new();

        /* Run Dijkstra's algorithm using comp length as edge weights */

        // Populate the frontier with all the possible start nodes, each with distance 0
        let start_nodes = layout
            .starts
            .iter()
            .map(|(start_course_head, start_row_idx, _name)| {
                NodeId::new(start_course_head.to_owned(), *start_row_idx, true)
            })
            .collect_vec();
        frontier.extend(
            start_nodes
                .iter()
                .cloned()
                .map(|id| FrontierNode(id, 0))
                .map(Reverse),
        );

        // Consume nodes from the frontier until the frontier is empty
        while let Some(Reverse(FrontierNode(node_id, distance))) = frontier.pop() {
            // Don't expand nodes multiple times (Dijkstra's algorithm makes sure that the first time
            // it is expanded will be have the shortest distance)
            if expanded_nodes.get(&node_id).is_some() {
                continue;
            }
            // If the node hasn't been expanded yet, then add its reachable nodes to the frontier
            let segment = layout
                .get_segment(&node_id)
                .expect("Infinite segment found");

            // If the shortest composition including this node is longer the length limit, then don't
            // include it in the node graph
            let new_dist = distance + segment.length;
            if new_dist > max_length {
                continue;
            }
            // Expand the node by adding its successors to the frontier
            for (_link_idx, id_after_link) in &segment.links {
                // Add the new node to the frontier
                frontier.push(Reverse(FrontierNode(id_after_link.to_owned(), new_dist)));
            }
            // Mark this node as expanded
            expanded_nodes.insert(node_id, (segment, distance));
        }

        // Once Dijkstra's finishes, `expanded_nodes` contains every node reachable from rounds
        // within the length of the composition.  However, we're still not done because we have to
        // build a graph over these IDs (which requires computing falseness, music, connections,
        // etc.).
        let mut nodes: HashMap<NodeId, Node> = expanded_nodes
            .iter()
            .map(|(node_id, (segment, distance))| {
                let score = Breakdown::from_rows(
                    segment.untransposed_rows(layout),
                    &node_id.course_head,
                    music_types,
                );

                let new_node = Node {
                    length: segment.length,
                    score,

                    start_idx: segment.start_idx,
                    end_idx: segment.end_idx,

                    min_distance_from_rounds: *distance,
                    // Distances to rounds are computed during optimisation
                    min_distance_to_rounds: None,

                    successors: segment.links.to_owned(),
                    // These are populated in separate passes over the graph
                    false_nodes: Vec::new(),
                    predecessors: Vec::new(),
                };
                (node_id.clone(), new_node)
            })
            .collect();

        // We need to clone the `NodeId`s, because otherwise they would borrow from `nodes` whilst
        // the loop is modifying the contents (i.e. breaking reference aliasing)
        let node_ids_and_lengths = nodes
            .iter()
            .map(|(id, node)| (id.to_owned(), node.length))
            .collect_vec();

        // Compute falseness between the nodes
        let table = FalsenessTable::from_layout(layout, &node_ids_and_lengths);
        for (id, node) in nodes.iter_mut() {
            node.false_nodes = node_ids_and_lengths
                .iter()
                .filter(|(id2, length2)| table.are_false(id, node.length, id2, *length2))
                .map(|(id2, _)| id2.to_owned())
                .collect_vec();
        }

        // Add predecessor references
        for (id, _dist) in expanded_nodes {
            // I wish there was a way to do this without cloning the node IDs, but alas the borrow
            // checker won't let me.  In future, we should allocate the node IDs into an arena (or
            // use RCs) to make the cloning cheaper
            for (_, succ_id) in nodes.get(&id).unwrap().successors.clone() {
                if let Some(node) = nodes.get_mut(&succ_id) {
                    node.predecessors.push(id.clone());
                }
            }
        }

        Self { nodes, start_nodes }
    }

    /// Run Dijkstra's algorithm to update `min_distance_from_rounds` on every node
    fn recompute_distances_from_rounds(&mut self) {
        // Initialise the frontier with all the starting nodes (with distance 0)
        let mut frontier: BinaryHeap<Reverse<FrontierNode>> = self
            .start_nodes
            .iter()
            .map(|id| Reverse(FrontierNode(id.to_owned(), 0)))
            .collect();

        while let Some(Reverse(FrontierNode(node_id, distance))) = frontier.pop() {
            let node = match self.nodes.get_mut(&node_id) {
                Some(n) => n,
                // Skip this node if it's already been removed from the graph
                None => continue,
            };

            // If this doesn't improve the existing min distance, then don't bother updating this
            // node's successors
            if distance >= node.min_distance_from_rounds {
                continue;
            }
            // Update the new (improved) min distance
            node.min_distance_from_rounds = distance;

            let dist_after_node = distance + node.length;
            // Expand the node by adding its successors to the frontier
            for (_, succ_id) in &node.successors {
                // Add the new node to the frontier
                frontier.push(Reverse(FrontierNode(succ_id.clone(), dist_after_node)));
            }
        }
    }

    /// Run Dijkstra's algorithm to update `min_distance_from_rounds` on every node
    fn recompute_distances_to_rounds(&mut self) {
        // Initialise the frontier with all the end nodes.  Note that these distances go from the
        // **END** of the nodes
        let mut frontier: BinaryHeap<Reverse<FrontierNode>> = self
            .end_nodes()
            .map(|(id, _node)| Reverse(FrontierNode(id.clone(), 0)))
            .collect();

        while let Some(Reverse(FrontierNode(node_id, distance_from_end))) = frontier.pop() {
            let node = match self.nodes.get_mut(&node_id) {
                Some(n) => n,
                // Skip this node if it's already been removed from the graph
                None => continue,
            };

            // If this doesn't improve the existing min distance, then don't bother updating this
            // node's successors
            let distance_from_start = distance_from_end + node.length;
            if node
                .min_distance_to_rounds
                .map_or(false, |existing_dist| distance_from_start >= existing_dist)
            {
                continue;
            }
            // Update the new (improved) min distance
            node.min_distance_to_rounds = Some(distance_from_start);

            // Expand the node by adding its predecessors to the frontier
            for pred_id in &node.predecessors {
                // Add the new node to the frontier
                frontier.push(Reverse(FrontierNode(pred_id.clone(), distance_from_start)));
            }
        }
    }

    /// Remove any nodes which can't be included in any round-block composition short enough to fit
    /// within the max length.
    ///
    /// # Panics
    ///
    /// This panics if `self.recompute_distances_to_rounds` hasn't been run before this
    fn remove_nodes_by_distance(&mut self, max_length: usize) {
        /* Remove unreachable nodes - unreachable nodes can't be in any compositions */

        let mut reachable_nodes: HashSet<NodeId> = HashSet::with_capacity(self.nodes.len());

        // Run DFA on the graph starting from rounds, adding the nodes to `reachable_nodes`
        for id in &self.start_nodes {
            self.explore(id, &mut reachable_nodes);
        }

        // Remove any nodes in the graph which aren't in `reachable_nodes` (by adding them to
        // `nodes_to_remove` whilst initialising)
        let mut nodes_to_remove: HashSet<NodeId> = self
            .nodes
            .keys()
            .filter(|id| !reachable_nodes.contains(id))
            .cloned()
            .collect();
        nodes_to_remove.reserve(self.nodes.len());

        /* Remove nodes which can only exist in comps that are too long */

        for (id, node) in &self.nodes {
            match node.min_distance_to_rounds {
                // If the shortest composition going through this node is longer than the max
                // length, then this node can't be reachable in a short enough composition and we
                // can prune
                Some(dist_to_rounds) => {
                    let min_comp_length = node.min_distance_from_rounds + dist_to_rounds;
                    if min_comp_length > max_length {
                        nodes_to_remove.insert(id.clone());
                    }
                }
                // Either the node can't reach rounds (and can be removed from the graph), or this
                // hasn't been run after `recompute_distances_to_rounds` (which we can detect by
                // checking if end nodes are given a distance, and if not then panic).
                None => {
                    if node.is_end() {
                        // If an end node wasn't given a distance, then the distances to rounds are
                        // presumed invalid
                        panic!("End node marked as being unable to reach rounds!");
                    }
                    nodes_to_remove.insert(id.clone());
                }
            }
        }

        self.remove_nodes(nodes_to_remove);
    }

    fn explore(&self, id: &NodeId, reachable_nodes: &mut HashSet<NodeId>) {
        // Only expand the node if it exists
        if let Some(node) = self.nodes.get(id) {
            // Check if this node has been reached before, and if so don't expand it (this makes
            // sure every node is visited at most once, guaranteeing termination).
            if reachable_nodes.contains(id) {
                return;
            }
            // If the node hasn't been reached yet, then mark it as reached and explore its
            // successors
            reachable_nodes.insert(id.clone());
            for (_, succ_id) in &node.successors {
                self.explore(succ_id, reachable_nodes);
            }
        }
    }

    /// Remove cross-node references (falseness, successor, predecessor, etc.) which don't point to
    /// existing nodes.
    fn prune_references(&mut self) {
        // Lookup table for which node IDs actually correspond to nodes
        let node_ids: HashSet<NodeId> = self.nodes.keys().cloned().collect();

        for node in self.nodes.values_mut() {
            node.successors.retain(|(_, id)| node_ids.contains(id));
            node.predecessors.retain(|id| node_ids.contains(id));
            node.false_nodes.retain(|id| node_ids.contains(id));
        }
    }

    /// Reorder the successor links by the amount of easily reachable music - meaning that the
    /// DFS engine will expand the branches in roughly descending order of goodness.  This is done
    /// in dynamic programming style - ignoring falseness but using time and memory that is linear
    /// in both the number of node links and the search depth.
    fn sort_successors(&mut self, config: &Config) {
        // If the depth is 0, then the nodes should be sorted
        if config.successor_link_sort_depth == 0 {
            return;
        }

        /* COMPUTE THE REACHABLE MUSIC FOR EVERY NODE */

        // A mapping from Node IDs to average scores reachable within `i` nodes.  We initialise
        // this to 0
        let mut scores_at_i: HashMap<NodeId, Score> = self
            .nodes
            .keys()
            .map(|id| (id.clone(), Score::ZERO))
            .collect();
        // A mapping from Node IDs to average scores reachable within `i - 1` nodes
        let mut scores_at_i_minus_1 = HashMap::<NodeId, Score>::new();

        // Since `i` starts at 2, we need to populate the back buffer with the average score
        // reachable within `2 - 1 = 1` nodes.  This only allows for each node to explore itself,
        // and so each node is given its own score
        scores_at_i_minus_1.extend(
            self.nodes
                .iter()
                .map(|(id, node)| (id.clone(), node.score.total)),
        );

        /// Combine several music scores together
        fn combine_node_scores(
            vs: impl IntoIterator<Item = Score>,
            num_successors: usize,
            strat: SuccSortStrat,
        ) -> Score {
            match strat {
                SuccSortStrat::Max => vs.into_iter().max().unwrap_or(Score::ZERO),
                SuccSortStrat::Average => {
                    // If there are no successor nodes, then we arbitrarily set the average to 0 to
                    // avoid division by 0
                    if num_successors == 0 {
                        Score::ZERO
                    } else {
                        vs.into_iter().sum::<Score>() / num_successors
                    }
                }
            }
        }

        // Now, in order to increase the depth by one we overwrite the scores in
        // `reachable_music_buf_front` (the scores at depth `i`) by averaging the scores of each
        // node's successors taken from `reachable_music_buf_back` (the scores at depth `i - 1`).
        for _i in 2..=config.successor_link_sort_depth {
            for (id, node) in &self.nodes {
                // The average music reachable from a node is its score + the average of its
                // successor's music reachable in length `i - 1`
                let reachable_score = node.score.total
                    + combine_node_scores(
                        node.successors
                            .iter()
                            .map(|(_, succ_id)| *scores_at_i_minus_1.get(succ_id).unwrap()),
                        node.successors.len(),
                        config.successor_link_sort_strategy,
                    );
                // Update the front buffer with the new score
                *scores_at_i.get_mut(id).unwrap() = reachable_score;
            }
            // Swap the buffers so that the front buffer is now on the back
            std::mem::swap(&mut scores_at_i, &mut scores_at_i_minus_1);
        }

        /* REORDER THE SUCCESSOR LINKS */

        // At this point 'i = MAX_REORDER_DEPTH + 1' and the buffers have just been swapped, so we
        // want to read the scores out of the map for `i - 1`
        for (_id, node) in self.nodes.iter_mut() {
            node.successors.sort_by(|(_, id1), (_, id2)| {
                let score1 = *scores_at_i_minus_1.get(id1).unwrap();
                let score2 = *scores_at_i_minus_1.get(id2).unwrap();
                score1
                    .partial_cmp(&score2)
                    .expect("Unorderable score was found")
                    // We want the best scores at the front, so we reverse the ordering
                    .reverse()
            });
        }
    }

    /* ===== OPTIMISATION HELPER FUNCTIONS ===== */

    fn end_nodes(&self) -> impl Iterator<Item = (&NodeId, &Node)> {
        self.nodes.iter().filter(|(_id, node)| node.is_end())
    }

    /// Remove nodes from the graph by reference
    ///
    /// # Panics
    ///
    /// Panics if any ids point to non-existent nodes
    fn remove_nodes(&mut self, ids: impl IntoIterator<Item = NodeId>) {
        for id in ids {
            assert!(self.nodes.remove(&id).is_some());
        }
    }

    /* ===== UTILITY FUNCTIONS FOR THE REST OF THE CODE ===== */

    /// Converts a sequence of successor indexes into a sequence of [`NodeId`] and [`SegmentLink`]
    /// indexes (i.e. ones which refer to the indices as used in the [`Layout`]).  This is
    /// basically the conversion from the fast-but-obtuse format used in the composing loop to a
    /// more long-term usable format that can be used to generate human friendly representations of
    /// the compositions.
    pub fn generate_path(
        &self,
        start_idx: usize,
        succ_idxs: impl IntoIterator<Item = usize>,
        layout: &Layout,
    ) -> (usize, Vec<usize>, usize) {
        // Start the traversal at the start node
        let mut node_id = &{
            let (start_ch, start_row_idx, _start_name) = &layout.starts[start_idx];
            NodeId::new(start_ch.to_owned(), *start_row_idx, true)
        };
        // Follow succession references until the path finishes
        let path = succ_idxs
            .into_iter()
            .map(|succ_idx| {
                let (link_idx, succ_id) = &self.nodes.get(node_id).unwrap().successors[succ_idx];
                node_id = succ_id;
                *link_idx
            })
            .collect_vec();
        // Get the `end_idx` from the last node of the path (which, if this is a complete
        // composition, should be an end node)
        let end_idx = self
            .nodes
            .get(node_id)
            .unwrap()
            .end_idx
            .expect("Composition should end at an end node");
        // Return the whole path
        (start_idx, path, end_idx)
    }

    /// Gets the distance to rounds of a given [`NodeId`] (if a corresponding [`ProtoNode`]
    /// exists).
    pub fn get_min_dist_from_end_to_rounds(&self, node_id: &NodeId) -> Option<usize> {
        self.nodes
            .get(node_id)
            // `n.min_distance_to_rounds` can't be `None` because all distances are generated
            // during optimisation, and a value of `None` would cause the node to be removed from
            // the graph.
            //
            // Also, note that we subtract the node's length so that the distance refers to the
            // first row **after** this node.
            .map(|n| n.min_distance_to_rounds.unwrap() - n.length)
    }

    /// Creates `num_prefixes` unique prefixes which are as short as possible (i.e. distribute the
    /// composing work as evenly as possible).  **NOTE**: This doesn't check the truth of the
    /// resulting prefixes (yet), so it's worth generating more prefixes than you have threads.
    pub(crate) fn generate_prefixes(&self, num_prefixes: usize) -> VecDeque<QueueElem> {
        // We calculate the prefixes by running BFS on the graph until the frontier becomes larger
        // than `num_prefixes` in length, at which point it becomes our prefix list.
        let num_start_nodes = self.start_nodes.len();
        let mut frontier: VecDeque<(QueueElem, &Node)> = self
            .nodes
            .iter()
            .filter_map(|(_id, node)| {
                node.start_idx
                    .map(|start_idx| (QueueElem::just_start_node(start_idx, num_start_nodes), node))
            })
            .collect();

        // TODO: Expand evenly between all the different start nodes

        // Repeatedly expand prefixes until the frontier is large enough
        while let Some((prefix, node)) = frontier.pop_front() {
            for (succ_idx, (_, succ_id)) in node.successors.iter().enumerate() {
                if let Some(succ_node) = self.nodes.get(succ_id) {
                    // Extend the prefix with the new successor index
                    let mut new_prefix = prefix.clone();
                    new_prefix.push(succ_idx, node.successors.len());
                    // Add the new prefix to the back of the frontier
                    frontier.push_back((new_prefix, succ_node));
                } else {
                    println!("ERROR! Successor node {} doesn't exist", succ_id);
                }
            }

            if frontier.len() >= num_prefixes {
                break;
            }
        }

        frontier.into_iter().map(|(prefix, _node)| prefix).collect()
    }

    /// Compute the percentage (of the overall search) left to be computed after some prefix below
    /// a certain depth
    pub(crate) fn compute_percentage(&self, prefix: &CompPrefix, min_depth: usize) -> f64 {
        let mut percentage = 0f64;

        // Traverse down the prefix, dividing the percentage equally between the branches at each
        // depth
        let mut percentage_covered_by_node = 100.0 / self.start_nodes.len() as f64;
        let mut node_id = &self.start_nodes[prefix.start_idx];
        for (depth, &succ_idx) in prefix.successor_idxs.iter().enumerate() {
            let node = &self.nodes[node_id];
            let percentage_per_successor =
                percentage_covered_by_node / node.successors.len() as f64;

            if depth >= min_depth {
                percentage += percentage_per_successor * succ_idx as f64;
            }

            // Move to the successor, and divide the percentage
            node_id = &node.successors[succ_idx].1;
            percentage_covered_by_node = percentage_per_successor;
        }

        percentage
    }
}

/// A node in a prototype graph
#[derive(Debug, Clone)]
pub(super) struct Node {
    pub start_idx: Option<usize>,
    pub end_idx: Option<usize>,

    /// The number of rows in this node
    pub length: usize,
    /// The music generated by this node in the composition
    pub score: Breakdown,
    pub false_nodes: Vec<NodeId>,

    /// A lower bound on the number of rows required to go from any rounds to the first row of
    /// `self`
    min_distance_from_rounds: usize,
    /// A lower bound on the number of rows required to go from the first row of `self` to rounds
    /// (or `None` if rounds is unreachable - a distance of infinity - or the distances haven't
    /// been computed yet).
    min_distance_to_rounds: Option<usize>,

    /// The `usize` here denotes which link in the [`Layout`] has generated this succession.  This
    /// is required so that a human-friendly representation of the composition can be generated.
    pub successors: Vec<(usize, NodeId)>,
    pub predecessors: Vec<NodeId>,
}

impl Node {
    /// Is this `ProtoNode` a start node?
    pub fn is_end(&self) -> bool {
        self.end_idx.is_some()
    }
}

/// An orderable type for storing nodes on Dijkstra's Algorithm's frontier
#[derive(Debug, Clone, Eq, PartialEq, Hash)]
struct FrontierNode(NodeId, usize);

impl PartialOrd for FrontierNode {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for FrontierNode {
    fn cmp(&self, other: &Self) -> Ordering {
        self.1.cmp(&other.1)
    }
}