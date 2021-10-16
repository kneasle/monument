use std::collections::HashSet;

use crate::{Data, Graph};

/// Removes node references which point to non-existent nodes.  This cannot fail.
pub(super) fn strip_refs(graph: &mut Graph, _data: &Data) {
    // Cloned set of which `NodeID`s are in the graph.  We need to clone these, because otherwise
    // we'd have to borrow the node map whilst mutably iterating over it.
    let node_ids = graph.ids().cloned().collect::<HashSet<_>>();

    // Strip start/end nodes
    graph.retain_start_nodes(|id| node_ids.contains(id));
    graph.retain_end_nodes(|id| node_ids.contains(id));
    // Strip node refs (i.e. predecessor, successor or falseness)
    for (_id, node) in graph.nodes_mut() {
        node.successors_mut()
            .retain(|(_link_idx, id)| node_ids.contains(id));
        node.predecessors_mut()
            .retain(|(_link_idx, id)| node_ids.contains(id));
        node.false_nodes_mut().retain(|id| node_ids.contains(id));
    }

    // TODO: Strip `successor` ptrs which don't have a matching `predecessor` (or vice versa)
}