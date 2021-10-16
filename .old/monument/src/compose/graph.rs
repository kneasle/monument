//! Code for handling [`Graph`]s of [`Node`]s.  This is the only part of the Monument that knows
//! the details of how the node graph is represented in memory, and provides a safe interface to
//! that graph to the rest of the code.

use std::{alloc, collections::HashMap, fmt::Debug, pin::Pin, ptr::addr_of_mut};

use itertools::Itertools;

use crate::{
    graph::{Graph, Node},
    score::Score,
    spec::layout::NodeId,
};

/// An in-memory graph of [`Node`]s which is explored to find compositions.  Each [`Node`] carries
/// a custom payload (`P`), which can be used as annotations.
#[derive(Debug)]
pub(crate) struct PtrGraph<P, E> {
    /// The set of nodes reachable in the graph.  For the rest of the code, these are entirely
    /// owned by the `Graph` (which acts like an arena for dynamically sized [`Node`]s).
    nodes: HashMap<NodeId, Pin<Box<PtrNode<P, E>>>>,
    /// The [`Node`]s which can start a composition, referred to by their index from
    /// [`Layout::starts`]
    start_nodes: HashMap<usize, *const PtrNode<P, E>>,
}

impl<P, E> PtrGraph<P, E> {
    /// Create a new `Graph` containing enough nodes to generate all comps of the [`Engine`].
    pub fn from_prototype(
        prototype_graph: &Graph,
        mut gen_payload: impl FnMut(&NodeId) -> P,
        mut gen_extra_payload: impl FnMut(&NodeId) -> E,
    ) -> Self
    where
        P: Unpin,
    {
        // Check that the only nodes with no successors are the end nodes (this is used by the
        // composing loop as a way to test for end nodes without storing extra data in the
        // `NodePayload`)
        for node in prototype_graph.nodes.values() {
            assert_eq!(node.successors.is_empty(), node.is_end());
        }
        // For each reachable node ID, generate a blank node (i.e. a node where all the
        // succession/falseness pointers are `null`, but everything else is initialised) along with
        // additional information about which successors/false nodes are reachable
        let nodes: HashMap<&NodeId, (*mut PtrNode<P, E>, &Node)> = prototype_graph
            .nodes
            .iter()
            .map(|(id, proto_node)| {
                (
                    id,
                    (
                        PtrNode::blank(
                            gen_payload(&id),
                            gen_extra_payload(&id),
                            id.clone(),
                            proto_node.length,
                            proto_node.score.total,
                            proto_node.successors.len(),
                            proto_node.false_nodes.len(),
                        ),
                        proto_node,
                    ),
                )
            })
            .collect();

        // Now that all the nodes have been allocated, set the succession/falseness pointers and
        // initialise the link map
        for (_id, (node_ptr, proto_node)) in nodes.iter() {
            let node = unsafe { node_ptr.as_mut() }.unwrap();

            // Set the successor pointers and link mapping
            let succ_ptrs = node.successors_mut();
            for ((_succ_idx, succ_id), succ_ptr) in
                proto_node.successors.iter().zip_eq(succ_ptrs.iter_mut())
            {
                *succ_ptr = nodes.get(&succ_id).unwrap().0;
            }

            // Set the falseness pointers
            let false_ptrs = node.false_nodes_mut();
            for (false_id, false_ptr) in proto_node.false_nodes.iter().zip_eq(false_ptrs.iter_mut())
            {
                *false_ptr = nodes.get(&false_id).unwrap().0;
            }

            node.extras.link_map.extend(
                proto_node
                    .successors
                    .iter()
                    .map(|(succ_idx, _succ_id)| *succ_idx),
            );
        }

        // Before returning the nodes, check that the `*const Node<P>` and `&Node<P>` can be safely
        // transmuted (if they can't then traversing the graph causes UB)
        assert_eq!(
            std::alloc::Layout::new::<*const PtrNode<P, E>>(),
            std::alloc::Layout::new::<&PtrNode<P, E>>()
        );

        // Get the pointers for the possible starting nodes, along with the indices pointing to
        // which starts they correspond to
        let start_nodes: HashMap<usize, *const PtrNode<P, E>> = nodes
            .iter()
            .filter_map(|(_id, (node, proto_node))| {
                proto_node
                    .start_idx
                    .map(|idx| (idx, *node as *const PtrNode<P, E>))
            })
            .collect();

        // Now that we've initialised all the nodes, we wrap the nodes into pinned boxes (so that
        // they're owned but can't be moved) and return
        Self {
            start_nodes,
            nodes: nodes
                .into_iter()
                .map(|(id, (ptr, _))| (id.clone(), Pin::new(unsafe { Box::from_raw(ptr) })))
                .collect(),
        }
    }

    /// Gets a start node by ID, returning `None` if no start nodes have that [`NodeId`].
    pub fn get_start_node(&self, idx: usize) -> Option<&PtrNode<P, E>> {
        self.start_nodes
            .get(&idx)
            .map(|ptr| unsafe { ptr.as_ref() }.unwrap())
    }

    /// Gets a start node by ID, returning `None` if no start nodes have that [`NodeId`].
    // This function is used in code that only compiles on debug builds, so without this
    // suppression a (pointless) warning is generated when compiling in release mode
    #[allow(dead_code)]
    pub fn all_nodes(&self) -> impl Iterator<Item = Pin<&'_ PtrNode<P, E>>> {
        self.nodes.values().map(|x| x.as_ref())
    }
}

/// A node in the in-memory graph.  This is a dynamically sized type, since it contains a
/// dynamically sized array of pointers to other nodes.
#[derive(Debug)]
// repr(C) guarantees that the pointer array is the last element (and thus can be extended whist
// allocating).
#[repr(C)]
pub(crate) struct PtrNode<P, E> {
    // NOTE: If you add fields to this, you must remember to initialise them in `Node::blank`
    // otherwise their values will be undefined.
    payload: P,
    /// The number of rows in this node
    length: usize,
    extras: Box<ExtraNode<E>>,
    /// The music score generated by this node
    score: Score,

    num_successors: usize,
    num_false_nodes: usize,
    /// An inline array of pointers to other nodes.  This array is extended whilst allocating, and
    /// has length `num_successors + num_false_nodes`.
    ///
    /// - The first `num_successors` pointers refer to successor nodes
    /// - The next `num_false_nodes` pointers refer to false nodes
    ptrs: [*const Self; 1],
}

impl<P, E> PtrNode<P, E> {
    /// Allocate a `Node` where all the successor pointers are [`null`](std::ptr::null).
    fn blank(
        payload: P,
        extra_payload: E,
        id: NodeId,
        length: usize,
        score: Score,
        num_successors: usize,
        num_false_nodes: usize,
    ) -> *mut Self {
        let num_pointers = num_successors + num_false_nodes;
        // The size and alignment of a `Node` with one pointer
        let initial_size = std::mem::size_of::<Self>();
        let align = std::mem::align_of::<Self>();
        // The size of the node once the array is expanded (the `max` here ensures `Node.ptrs`
        // always has at least one element, so the unexpanded version is always safe to use).
        let extended_size =
            initial_size + num_pointers.saturating_sub(1) * std::mem::size_of::<*const Self>();

        // Allocate some uninitialised memory for the new node
        let extended_layout = alloc::Layout::from_size_align(extended_size, align).unwrap();
        let new_node = unsafe { alloc::alloc(extended_layout) as *mut Self };
        // Check the alignment of the new memory
        assert!(new_node as usize % align == 0);

        /// Macro to initialise a field of the new node, without causing LLVM to generate `ud2`
        /// instructions when used in combination with `Box` (for `Node.extra`)
        macro_rules! init_field {
            ($field: ident, $value: expr) => {
                unsafe {
                    std::ptr::write(addr_of_mut!((*new_node).$field), $value);
                }
            };
        }

        // Initialise the new node
        init_field!(payload, payload);
        init_field!(length, length);
        init_field!(score, score);
        init_field!(
            extras,
            Box::new(ExtraNode {
                payload: extra_payload,
                id,
                link_map: Vec::with_capacity(num_successors),
            })
        );
        init_field!(num_successors, num_successors);
        init_field!(num_false_nodes, num_false_nodes);

        // Initialise *all* the pointers to `null`.  This way, we can easily check if the nodes
        // have been correctly initialised.
        unsafe {
            for i in 0..num_pointers {
                *((*new_node).ptrs.get_unchecked_mut(i) as *mut *const Self) = std::ptr::null_mut();
            }
        }

        new_node
    }

    #[inline(always)]
    pub fn payload(&self) -> &P {
        &self.payload
    }

    #[inline(always)]
    pub fn length(&self) -> usize {
        self.length
    }

    #[inline(always)]
    pub fn score(&self) -> Score {
        self.score
    }

    // This function is used in code that only compiles on debug builds, so without this
    // suppression a (pointless) warning is generated when compiling in release mode
    #[allow(dead_code)]
    #[inline(always)]
    pub fn id(&self) -> &NodeId {
        &self.extras.id
    }

    #[inline(always)]
    pub fn is_end(&self) -> bool {
        // This is a necessary condition for a node being an end, because the node graph optimiser
        // would remove any non-end nodes with no successors (because such nodes would never be
        // able to reach rounds).  This is checked in `Graph::from_prototype`.
        self.num_successors == 0
    }

    pub fn successors(&self) -> &[&Self] {
        // This unsafety is OK because:
        // - we allocated the extended slice within a single allocation
        // - we initialised the entire contents of the extended slice
        unsafe {
            std::slice::from_raw_parts(self.ptrs.as_ptr() as *const &Self, self.num_successors)
        }
    }

    fn successors_mut(&mut self) -> &mut [*const Self] {
        // This unsafety is OK because:
        // - we allocated the extended slice within a single allocation
        // - we initialised the entire contents of the extended slice
        unsafe { std::slice::from_raw_parts_mut(self.ptrs.as_mut_ptr(), self.num_successors) }
    }

    pub fn false_nodes(&self) -> &[&Self] {
        // This unsafety is OK because:
        // - we allocated the extended slice within a single allocation
        // - we initialised the entire contents of the extended slice
        unsafe {
            std::slice::from_raw_parts(
                self.ptrs.as_ptr().add(self.num_successors) as *const &Self,
                self.num_false_nodes,
            )
        }
    }

    fn false_nodes_mut(&mut self) -> &mut [*const Self] {
        // This unsafety is OK because:
        // - we allocated the extended slice within a single allocation
        // - we initialised the entire contents of the extended slice
        unsafe {
            std::slice::from_raw_parts_mut(
                self.ptrs.as_mut_ptr().add(self.num_successors),
                self.num_false_nodes,
            )
        }
    }
}

/// Extra data for a [`Node`] that isn't required by the composing loop and is so has one extra
/// level of indirection.
#[derive(Debug, Clone)]
pub struct ExtraNode<P> {
    payload: P,
    /// The ID of this node
    id: NodeId,
    /// For a given successor pointer (at index `i`), `link_map[i]` denotes which [`SegmentLink`]
    /// it originally refers to (because links may be reordered or removed during graph
    /// optimisation).
    link_map: Vec<usize>,
}