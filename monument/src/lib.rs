pub mod graph;
pub mod layout;
pub mod music;
mod search;
mod utils;

pub use search::Comp;
pub use utils::OptRange;

use std::{
    ops::Range,
    sync::{Arc, Mutex},
};

use bellframe::RowBuf;
use graph::{optimise::Pass, Graph};
use itertools::Itertools;
use log::log;

/// Information provided to Monument which specifies what compositions are generated.
///
/// Compare this to [`Config`], which determines _how_ those compositions are generated (and
/// therefore determines how quickly the results are generated).
#[derive(Debug, Clone)]
pub struct Query {
    pub layout: layout::Layout,
    pub music_types: Vec<music::MusicType>,
    pub part_head: RowBuf,
    pub len_range: Range<usize>,
    pub method_count_range: Range<usize>,
    pub num_comps: usize,
}

/// Configuration parameters for Monument which **don't** change which compositions are emitted.
pub struct Config {
    /// Number of threads used to generate compositions.  If `None`, this uses the number of
    /// **physical** CPU cores (i.e. ignoring hyper-threading).
    pub num_threads: Option<usize>,
    pub queue_limit: usize,
    pub optimisation_passes: Vec<Pass>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            num_threads: None,
            queue_limit: 10_000_000,
            optimisation_passes: graph::optimise::passes::default(),
        }
    }
}

////////////
// SEARCH //
////////////

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebugOutput {
    /// Return the unoptimised [`Graph`]
    Graph,
    /// Stop just before the search starts, to let the user see what's been printed out without
    /// scrolling
    StopBeforeSearch,
}

pub fn run_query(
    query_arc: Arc<Query>,
    config: &mut Config,
    debug_output: Option<DebugOutput>,
) -> Result<Vec<Comp>, Option<Graph>> {
    log::info!("Building `Graph`");
    let mut graph = query_arc.unoptimised_graph();
    if debug_output == Some(DebugOutput::Graph) {
        return Err(Some(graph)); // Return the graph if the caller wants to inspect it
    }

    log::debug!("Optimising graph");
    graph.optimise(&mut config.optimisation_passes, &query_arc);
    log::debug!(
        "{} nodes, {} starts, {} ends",
        graph.node_map().len(),
        graph.start_nodes().len(),
        graph.end_nodes().len()
    );

    if debug_output == Some(DebugOutput::StopBeforeSearch) {
        return Err(None);
    }

    log::info!("Starting tree search");
    let comps_arc = Arc::from(Mutex::new(Vec::<Comp>::new()));
    let graph_arc = Arc::from(graph);
    let num_threads = config.num_threads.unwrap_or_else(num_cpus::get_physical);
    let queue_limit = config.queue_limit;

    let handles = (0..num_threads)
        .map(|_i| {
            let query = query_arc.clone();
            let comps = comps_arc.clone();
            let graph = graph_arc.clone();
            std::thread::spawn(move || {
                let on_find_comp = |c: Comp| {
                    print_comp(&c, &query.layout);
                    comps.lock().unwrap().push(c);
                };
                search::search::<search::frontier::BestFirst<_>, _>(
                    &graph,
                    &query,
                    queue_limit / num_threads,
                    on_find_comp,
                );
            })
        })
        .collect_vec();
    // Wait for the worker threads to finish
    for h in handles {
        h.join().unwrap();
    }

    // Return all the comps in ascending order of goodness
    let mut comps = comps_arc.lock().unwrap().clone();
    comps.sort_by_key(|comp| comp.avg_score);
    Ok(comps)
}

impl Query {
    fn unoptimised_graph(&self) -> Graph {
        graph::Graph::from_layout(
            &self.layout,
            &self.music_types,
            // `- 1` makes sure that the length limit is an **inclusive** bound
            self.len_range.end - 1,
            &self.part_head,
        )
    }
}

fn print_comp(c: &Comp, layout: &layout::Layout) {
    println!(
        "len: {}, ms: {:>3?}, score: {:>6.2}, avg: {:.6}, rot: {}, str: {}",
        c.length,
        c.method_counts.counts(),
        c.score,
        c.avg_score,
        c.rotation,
        c.display_string(layout)
    );
}
