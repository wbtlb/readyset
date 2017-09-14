use backlog;
use channel;
use checktable;
use ops::base::Base;

use std::collections::HashMap;
use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use std::net::IpAddr;
use std::time;
use std::fmt;
use std::io;

use mio::net::TcpListener;

use slog;
use petgraph;
use petgraph::visit::Bfs;
use petgraph::graph::NodeIndex;

use tarpc::sync::client::{self, ClientExt};

pub mod coordination;
pub mod core;
pub mod debug;
pub mod domain;
pub mod hook;
pub mod keys;
pub mod migrate;
pub mod node;
pub mod payload;
pub mod persistence;
pub mod placement;
pub mod prelude;
pub mod statistics;

mod mutator;
mod getter;
mod transactions;

use self::prelude::{Ingredient, WorkerEndpoint, WorkerIdentifier};

pub use self::mutator::{Mutator, MutatorBuilder, MutatorError};
pub use self::getter::{Getter, ReadQuery, ReadReply, RemoteGetter, RemoteGetterBuilder};

const NANOS_PER_SEC: u64 = 1_000_000_000;
macro_rules! dur_to_ns {
    ($d:expr) => {{
        let d = $d;
        d.as_secs() * NANOS_PER_SEC + d.subsec_nanos() as u64
    }}
}

type Readers = Arc<Mutex<HashMap<(NodeIndex, usize), backlog::SingleReadHandle>>>;
pub type Edge = bool; // should the edge be materialized?

/// `Blender` is the core component of the alternate Soup implementation.
///
/// It keeps track of the structure of the underlying data flow graph and its domains. `Blender`
/// does not allow direct manipulation of the graph. Instead, changes must be instigated through a
/// `Migration`, which can be started using `Blender::start_migration`. Only one `Migration` can
/// occur at any given point in time.
pub struct Blender {
    ingredients: petgraph::Graph<node::Node, Edge>,
    source: NodeIndex,
    ndomains: usize,
    checktable: checktable::CheckTableClient,
    checktable_addr: SocketAddr,
    partial: HashSet<NodeIndex>,
    partial_enabled: bool,
    sharding_enabled: bool,

    /// Parameters for persistence code.
    persistence: persistence::Parameters,

    domains: HashMap<domain::Index, domain::DomainHandle>,
    channel_coordinator: Arc<prelude::ChannelCoordinator>,
    debug_channel: Option<SocketAddr>,

    listen_addr: IpAddr,
    readers: Readers,
    workers: HashMap<WorkerIdentifier, WorkerEndpoint>,
    remote_readers: HashMap<(domain::Index, usize), SocketAddr>,

    log: slog::Logger,
}

impl Default for Blender {
    fn default() -> Self {
        let mut g = petgraph::Graph::new();
        let source = g.add_node(node::Node::new(
            "source",
            &["because-type-inference"],
            node::special::Source,
            true,
        ));

        let checktable_addr = checktable::service::CheckTableServer::start();
        let checktable =
            checktable::CheckTableClient::connect(checktable_addr, client::Options::default())
                .unwrap();

        Blender {
            ingredients: g,
            source: source,
            ndomains: 0,
            checktable,
            checktable_addr,
            partial: Default::default(),
            partial_enabled: true,
            sharding_enabled: true,

            persistence: persistence::Parameters::default(),

            domains: Default::default(),
            channel_coordinator: Arc::new(prelude::ChannelCoordinator::new()),
            debug_channel: None,

            listen_addr: "127.0.0.1".parse().unwrap(),
            readers: Arc::default(),
            workers: HashMap::default(),
            remote_readers: HashMap::default(),

            log: slog::Logger::root(slog::Discard, o!()),
        }
    }
}

impl Blender {
    /// Construct a new, empty `Blender`
    pub fn new() -> Self {
        Blender::default()
    }

    /// Construct `Blender` with a specified listening interface
    pub fn with_listen(addr: IpAddr) -> Self {
        let mut b = Blender::default();
        b.listen_addr = addr;
        b
    }

    /// Disable partial materialization for all subsequent migrations
    pub fn disable_partial(&mut self) {
        self.partial_enabled = false;
    }

    /// Disable sharding for all subsequent migrations
    pub fn disable_sharding(&mut self) {
        self.sharding_enabled = false;
    }

    /// Adds another worker to host domains.
    pub fn add_worker(&mut self, addr: SocketAddr, sender: WorkerEndpoint) {
        if !self.workers.contains_key(&addr) {
            debug!(self.log, "added new worker {:?} to Blender", addr);
            self.workers.insert(addr.clone(), sender);
        } else {
            warn!(
                self.log,
                "worker {:?} already exists; ignoring request to add it!",
                addr
            );
        }
    }

    /// Return the number of workers currently registered.
    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    /// Tell the blender about a remote domain so that reads can be routed to the worker that
    /// maintains it.
    pub fn register_remote_domain(
        &mut self,
        index: domain::Index,
        shard: usize,
        read_addr: SocketAddr,
    ) {
        if !self.remote_readers.contains_key(&(index, shard)) {
            debug!(
                self.log,
                "added new remote domain {:?} with read_addr {:} to Blender",
                (index, shard),
                read_addr,
            );
            self.remote_readers.insert((index, shard), read_addr);
        } else {
            warn!(
                self.log,
                "remote domain {:?} already exists; ignoring request to add it!",
                (index, shard)
            );
        }
    }

    /// Use a debug channel. This function may only be called once because the receiving end it
    /// returned.
    pub fn create_debug_channel(&mut self) -> TcpListener {
        assert!(self.debug_channel.is_none());
        let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
        let listener = TcpListener::bind(&addr).unwrap();
        self.debug_channel = Some(listener.local_addr().unwrap());
        listener
    }

    /// Controls the persistence mode, and parameters related to persistence.
    ///
    /// Three modes are available:
    ///
    ///  1. `DurabilityMode::Permanent`: all writes to base nodes should be written to disk.
    ///  2. `DurabilityMode::DeleteOnExit`: all writes are written to disk, but the log is
    ///     deleted once the `Blender` is dropped. Useful for tests.
    ///  3. `DurabilityMode::MemoryOnly`: no writes to disk, store all writes in memory.
    ///     Useful for baseline numbers.
    ///
    /// `queue_capacity` indicates the number of packets that should be buffered until
    /// flushing, and `flush_timeout` indicates the length of time to wait before flushing
    /// anyway.
    ///
    /// Must be called before any domains have been created.
    pub fn with_persistence_options(&mut self, params: persistence::Parameters) {
        assert_eq!(self.ndomains, 0);
        self.persistence = params;
    }

    /// Set the `Logger` to use for internal log messages.
    ///
    /// By default, all log messages are discarded.
    pub fn log_with(&mut self, log: slog::Logger) {
        self.log = log;
    }

    /// Start setting up a new `Migration`.
    pub fn start_migration(&mut self) -> Migration {
        info!(self.log, "starting migration");
        let miglog = self.log.new(o!());
        Migration {
            mainline: self,
            added: Default::default(),
            columns: Default::default(),
            materialize: Default::default(),
            readers: Default::default(),

            start: time::Instant::now(),
            log: miglog,
        }
    }

    /// Get a boxed function which can be used to validate tokens.
    pub fn get_validator(&self) -> Box<Fn(&checktable::Token) -> bool> {
        let checktable =
            checktable::CheckTableClient::connect(self.checktable_addr, client::Options::default())
                .unwrap();
        Box::new(move |t: &checktable::Token| {
            checktable.validate_token(t.clone()).unwrap()
        })
    }

    #[cfg(test)]
    pub fn graph(&self) -> &prelude::Graph {
        &self.ingredients
    }

    /// Get references to all known input nodes.
    ///
    /// Input nodes are here all nodes of type `Base`. The addresses returned by this function will
    /// all have been returned as a key in the map from `commit` at some point in the past.
    ///
    /// This function will only tell you which nodes are input nodes in the graph. To obtain a
    /// function for inserting writes, use `Blender::get_putter`.
    pub fn inputs(&self) -> Vec<(prelude::NodeIndex, &node::Node)> {
        self.ingredients
            .neighbors_directed(self.source, petgraph::EdgeDirection::Outgoing)
            .map(|n| {
                let base = &self.ingredients[n];
                assert!(base.is_internal());
                assert!(base.get_base().is_some());
                (n.into(), base)
            })
            .collect()
    }

    /// Get a reference to all known output nodes.
    ///
    /// Output nodes here refers to nodes of type `Reader`, which is the nodes created in response
    /// to calling `.maintain` or `.stream` for a node during a migration.
    ///
    /// This function will only tell you which nodes are output nodes in the graph. To obtain a
    /// function for performing reads, call `.get_reader()` on the returned reader.
    pub fn outputs(&self) -> Vec<(prelude::NodeIndex, &node::Node)> {
        self.ingredients
            .externals(petgraph::EdgeDirection::Outgoing)
            .filter_map(|n| {
                self.ingredients[n].with_reader(|r| {
                    // we want to give the the node that is being materialized
                    // not the reader node itself
                    let src = r.is_for();
                    (src, &self.ingredients[src])
                })
            })
            .collect()
    }

    fn find_getter_for(&self, node: prelude::NodeIndex) -> Option<NodeIndex> {
        // reader should be a child of the given node. however, due to sharding, it may not be an
        // *immediate* child. furthermore, once we go beyond depth 1, we may accidentally hit an
        // *unrelated* reader node. to account for this, readers keep track of what node they are
        // "for", and we simply search for the appropriate reader by that metric. since we know
        // that the reader must be relatively close, a BFS search is the way to go.
        // presumably only
        let mut bfs = Bfs::new(&self.ingredients, node);
        let mut reader = None;
        while let Some(child) = bfs.next(&self.ingredients) {
            if self.ingredients[child]
                .with_reader(|r| r.is_for() == node)
                .unwrap_or(false)
            {
                reader = Some(child);
                break;
            }
        }

        reader
    }

    /// Obtain a `Getter` that allows querying a given (already maintained) reader node.
    pub fn get_getter(&self, node: prelude::NodeIndex) -> Option<Getter> {
        self.find_getter_for(node).and_then(|r| {
            let sharded = self.ingredients[r].sharded_by() != migrate::sharding::Sharding::None;
            Getter::new(r, sharded, &self.readers, &self.ingredients)
        })
    }

    /// Obtain a `RemoteGetterBuilder` that can be sent to a client and then used to query a given
    /// (already maintained) reader node.
    pub fn get_remote_getter_builder(
        &self,
        node: prelude::NodeIndex,
    ) -> Option<RemoteGetterBuilder> {
        self.find_getter_for(node).map(|r| {
            let domain = self.ingredients[r].domain();
            let shards = (0..self.domains[&domain].shards())
                .map(|i| self.remote_readers.get(&(domain, i)).unwrap().clone())
                .collect();

            RemoteGetterBuilder { node: r, shards }
        })
    }

    /// Convience method that obtains a MutatorBuilder and then calls build() on it.
    pub fn get_mutator(&self, base: prelude::NodeIndex) -> Mutator {
        self.get_mutator_builder(base).build()
    }

    /// Obtain a MutatorBuild that can be used to construct a Mutator to perform writes and deletes
    /// from the given base node.
    pub fn get_mutator_builder(&self, base: prelude::NodeIndex) -> MutatorBuilder {
        let node = &self.ingredients[base];

        trace!(self.log, "creating mutator"; "for" => base.index());

        let mut key = self.ingredients[base]
            .suggest_indexes(base)
            .remove(&base)
            .unwrap_or_else(Vec::new);
        let mut is_primary = false;
        if key.is_empty() {
            if let prelude::Sharding::ByColumn(col) = self.ingredients[base].sharded_by() {
                key = vec![col];
            }
        } else {
            is_primary = true;
        }


        let txs = (0..self.domains[&node.domain()].shards())
            .map(|i| {
                self.channel_coordinator
                    .get_addr(&(node.domain(), i))
                    .unwrap()
            })
            .collect();

        let num_fields = node.fields().len();
        let base_operator = node.get_base()
            .expect("asked to get mutator for non-base node");
        MutatorBuilder {
            txs,
            addr: (*node.local_addr()).into(),
            key: key,
            key_is_primary: is_primary,
            transactional: self.ingredients[base].is_transactional(),
            dropped: base_operator.get_dropped(),
            expected_columns: num_fields - base_operator.get_dropped().len(),
        }
    }

    /// Get statistics about the time spent processing different parts of the graph.
    pub fn get_statistics(&mut self) -> statistics::GraphStats {
        // TODO: request stats from domains in parallel.
        let domains = self.domains
            .iter_mut()
            .flat_map(|(di, s)| {
                s.send(box payload::Packet::GetStatistics).unwrap();
                s.wait_for_statistics()
                    .unwrap()
                    .into_iter()
                    .enumerate()
                    .map(move |(i, (domain_stats, node_stats))| {
                        let node_map = node_stats
                            .into_iter()
                            .map(|(ni, ns)| (ni.into(), ns))
                            .collect();

                        ((di.clone(), i), (domain_stats, node_map))
                    })
            })
            .collect();

        statistics::GraphStats { domains: domains }
    }
}

impl fmt::Display for Blender {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let indentln = |f: &mut fmt::Formatter| write!(f, "    ");

        // Output header.
        writeln!(f, "digraph {{")?;

        // Output global formatting.
        indentln(f)?;
        writeln!(f, "node [shape=record, fontsize=10]")?;

        // Output node descriptions.
        for index in self.ingredients.node_indices() {
            indentln(f)?;
            write!(f, "{}", index.index())?;
            self.ingredients[index].describe(f, index)?;
        }

        // Output edges.
        for (_, edge) in self.ingredients.raw_edges().iter().enumerate() {
            indentln(f)?;
            write!(f, "{} -> {}", edge.source().index(), edge.target().index())?;
            if !edge.weight {
                // not materialized
                writeln!(f, " [style=\"dashed\"]")?;
            } else {
                writeln!(f, "")?;
            }
        }

        // Output footer.
        write!(f, "}}")?;

        Ok(())
    }
}

enum ColumnChange {
    Add(String, prelude::DataType),
    Drop(usize),
}

/// A `Migration` encapsulates a number of changes to the Soup data flow graph.
///
/// Only one `Migration` can be in effect at any point in time. No changes are made to the running
/// graph until the `Migration` is committed (using `Migration::commit`).
pub struct Migration<'a> {
    mainline: &'a mut Blender,
    added: Vec<NodeIndex>,
    columns: Vec<(NodeIndex, ColumnChange)>,
    readers: HashMap<NodeIndex, NodeIndex>,
    materialize: HashSet<(NodeIndex, NodeIndex)>,

    start: time::Instant,
    log: slog::Logger,
}

impl<'a> Migration<'a> {
    /// Add the given `Ingredient` to the Soup.
    ///
    /// The returned identifier can later be used to refer to the added ingredient.
    /// Edges in the data flow graph are automatically added based on the ingredient's reported
    /// `ancestors`.
    pub fn add_ingredient<S1, FS, S2, I>(
        &mut self,
        name: S1,
        fields: FS,
        mut i: I,
    ) -> prelude::NodeIndex
    where
        S1: ToString,
        S2: ToString,
        FS: IntoIterator<Item = S2>,
        I: prelude::Ingredient + Into<prelude::NodeOperator>,
    {
        i.on_connected(&self.mainline.ingredients);
        let parents = i.ancestors();

        let transactional = !parents.is_empty() &&
            parents
                .iter()
                .all(|&p| self.mainline.ingredients[p].is_transactional());

        // add to the graph
        let ni = self.mainline.ingredients.add_node(node::Node::new(
            name.to_string(),
            fields,
            i.into(),
            transactional,
        ));
        info!(self.log,
              "adding new node";
              "node" => ni.index(),
              "type" => format!("{:?}", self.mainline.ingredients[ni])
        );

        // keep track of the fact that it's new
        self.added.push(ni);
        // insert it into the graph
        if parents.is_empty() {
            self.mainline
                .ingredients
                .add_edge(self.mainline.source, ni, false);
        } else {
            for parent in parents {
                self.mainline.ingredients.add_edge(parent, ni, false);
            }
        }
        // and tell the caller its id
        ni.into()
    }

    /// Add a transactional base node to the graph
    pub fn add_transactional_base<S1, FS, S2>(
        &mut self,
        name: S1,
        fields: FS,
        mut b: Base,
    ) -> prelude::NodeIndex
    where
        S1: ToString,
        S2: ToString,
        FS: IntoIterator<Item = S2>,
    {
        b.on_connected(&self.mainline.ingredients);
        let b: prelude::NodeOperator = b.into();

        // add to the graph
        let ni = self.mainline
            .ingredients
            .add_node(node::Node::new(name.to_string(), fields, b, true));
        info!(self.log,
              "adding new node";
              "node" => ni.index(),
              "type" => format!("{:?}", self.mainline.ingredients[ni])
        );

        // keep track of the fact that it's new
        self.added.push(ni);
        // insert it into the graph
        self.mainline
            .ingredients
            .add_edge(self.mainline.source, ni, false);
        // and tell the caller its id
        ni.into()
    }

    /// Add a new column to a base node.
    ///
    /// Note that a default value must be provided such that old writes can be converted into this
    /// new type.
    pub fn add_column<S: ToString>(
        &mut self,
        node: prelude::NodeIndex,
        field: S,
        default: prelude::DataType,
    ) -> usize {
        // not allowed to add columns to new nodes
        assert!(!self.added.iter().any(|&ni| ni == node));

        let field = field.to_string();
        let base = &mut self.mainline.ingredients[node];
        assert!(base.is_internal() && base.get_base().is_some());

        // we need to tell the base about its new column and its default, so that old writes that
        // do not have it get the additional value added to them.
        let col_i1 = base.add_column(&field);
        // we can't rely on DerefMut, since it disallows mutating Taken nodes
        {
            let col_i2 = base.inner_mut()
                .get_base_mut()
                .unwrap()
                .add_column(default.clone());
            assert_eq!(col_i1, col_i2);
        }

        // also eventually propagate to domain clone
        self.columns.push((node, ColumnChange::Add(field, default)));

        col_i1
    }

    /// Drop a column from a base node.
    pub fn drop_column(&mut self, node: prelude::NodeIndex, column: usize) {
        // not allowed to drop columns from new nodes
        assert!(!self.added.iter().any(|&ni| ni == node));

        let base = &mut self.mainline.ingredients[node];
        assert!(base.is_internal() && base.get_base().is_some());

        // we need to tell the base about the dropped column, so that old writes that contain that
        // column will have it filled in with default values (this is done in Mutator).
        // we can't rely on DerefMut, since it disallows mutating Taken nodes
        base.inner_mut().get_base_mut().unwrap().drop_column(column);

        // also eventually propagate to domain clone
        self.columns.push((node, ColumnChange::Drop(column)));
    }

    #[cfg(test)]
    pub fn graph(&self) -> &prelude::Graph {
        self.mainline.graph()
    }

    /// Mark the edge between `src` and `dst` in the graph as requiring materialization.
    ///
    /// The reason this is placed per edge rather than per node is that only some children of a
    /// node may require materialization of their inputs (i.e., only those that will query along
    /// this edge). Since we must materialize the output of a node in a foreign domain once for
    /// every receiving domain, this can save us some space if a child that doesn't require
    /// materialization is in its own domain. If multiple nodes in the same domain require
    /// materialization of the same parent, that materialized state will be shared.
    pub fn materialize(&mut self, src: prelude::NodeIndex, dst: prelude::NodeIndex) {
        // TODO
        // what about if a user tries to materialize a cross-domain edge that has already been
        // converted to an egress/ingress pair?
        let e = self.mainline
            .ingredients
            .find_edge(src, dst)
            .expect("asked to materialize non-existing edge");

        debug!(self.log, "told to materialize"; "node" => src.index());

        let e = self.mainline.ingredients.edge_weight_mut(e).unwrap();
        if !*e {
            *e = true;
            // it'd be nice if we could just store the EdgeIndex here, but unfortunately that's not
            // guaranteed by petgraph to be stable in the presence of edge removals (which we do in
            // commit())
            self.materialize.insert((src, dst));
        }
    }

    fn ensure_reader_for(&mut self, n: prelude::NodeIndex) {
        if !self.readers.contains_key(&n) {
            // make a reader
            let r = node::special::Reader::new(n);
            let r = self.mainline.ingredients[n].mirror(r);
            let r = self.mainline.ingredients.add_node(r);
            self.mainline.ingredients.add_edge(n, r, false);
            self.readers.insert(n, r);
        }
    }

    fn ensure_token_generator(&mut self, n: prelude::NodeIndex, key: usize) {
        let ri = self.readers[&n];
        if self.mainline.ingredients[ri]
            .with_reader(|r| r.token_generator().is_some())
            .expect("tried to add token generator to non-reader node")
        {
            return;
        }

        // A map from base node to the column in that base node whose value must match the value of
        // this node's column to cause a conflict. Is None for a given base node if any write to
        // that base node might cause a conflict.
        let base_columns: Vec<(_, Option<_>)> =
            keys::provenance_of(&self.mainline.ingredients, n, key, |_, _| None)
                .into_iter()
                .map(|path| {
                    // we want the base node corresponding to each path
                    path.into_iter().last().unwrap()
                })
                .collect();

        let coarse_parents = base_columns
            .iter()
            .filter_map(|&(ni, o)| if o.is_none() { Some(ni) } else { None })
            .collect();

        let granular_parents = base_columns
            .into_iter()
            .filter_map(|(ni, o)| if o.is_some() {
                Some((ni, o.unwrap()))
            } else {
                None
            })
            .collect();

        let token_generator = checktable::TokenGenerator::new(coarse_parents, granular_parents);
        self.mainline
            .checktable
            .track(token_generator.clone())
            .unwrap();

        self.mainline.ingredients[ri].with_reader_mut(|r| {
            r.set_token_generator(token_generator);
        });
    }

    /// Set up the given node such that its output can be efficiently queried.
    ///
    /// To query into the maintained state, use `Blender::get_getter` or
    /// `Blender::get_transactional_getter`
    pub fn maintain(&mut self, n: prelude::NodeIndex, key: usize) {
        self.ensure_reader_for(n);
        if self.mainline.ingredients[n].is_transactional() {
            self.ensure_token_generator(n, key);
        }

        let ri = self.readers[&n];

        self.mainline.ingredients[ri].with_reader_mut(|r| r.set_key(key));
    }

    /// Obtain a channel that is fed by the output stream of the given node.
    ///
    /// As new updates are processed by the given node, its outputs will be streamed to the
    /// returned channel. Node that this channel is *not* bounded, and thus a receiver that is
    /// slower than the system as a hole will accumulate a large buffer over time.
    pub fn stream(&mut self, n: prelude::NodeIndex) -> mpsc::Receiver<Vec<node::StreamUpdate>> {
        self.ensure_reader_for(n);
        let (tx, rx) = mpsc::channel();
        let mut tx = channel::StreamSender::from_local(tx);

        // If the reader hasn't been incorporated into the graph yet, just add the streamer
        // directly.
        let ri = self.readers[&n];
        let mut res = None;
        self.mainline.ingredients[ri].with_reader_mut(|r| {
            res = Some(r.add_streamer(tx));
        });
        tx = match res.unwrap() {
            Ok(_) => return rx,
            Err(tx) => tx,
        };


        // Otherwise, send a message to the reader's domain to have it add the streamer.
        let reader = &self.mainline.ingredients[self.readers[&n]];
        self.mainline
            .domains
            .get_mut(&reader.domain())
            .unwrap()
            .send(box payload::Packet::AddStreamer {
                node: *reader.local_addr(),
                new_streamer: tx,
            })
            .unwrap();

        rx
    }

    /// Set up the given node such that its output is stored in Memcached.
    pub fn memcached_hook(
        &mut self,
        n: prelude::NodeIndex,
        name: String,
        servers: &[(&str, usize)],
        key: usize,
    ) -> io::Result<prelude::NodeIndex> {
        let h = try!(hook::Hook::new(name, servers, vec![key]));
        let h = self.mainline.ingredients[n].mirror(h);
        let h = self.mainline.ingredients.add_node(h);
        self.mainline.ingredients.add_edge(n, h, false);
        Ok(h.into())
    }

    /// Commit the changes introduced by this `Migration` to the master `Soup`.
    ///
    /// This will spin up an execution thread for each new thread domain, and hook those new
    /// domains into the larger Soup graph. The returned map contains entry points through which
    /// new updates should be sent to introduce them into the Soup.
    pub fn commit(self) {
        info!(self.log, "finalizing migration"; "#nodes" => self.added.len());

        let log = self.log;
        let start = self.start;
        let mut mainline = self.mainline;
        let mut new: HashSet<_> = self.added.into_iter().collect();

        // Readers are nodes too.
        for (_parent, reader) in self.readers {
            new.insert(reader);
        }

        // Shard the graph as desired
        let mut swapped0 = if mainline.sharding_enabled {
            migrate::sharding::shard(&log, &mut mainline.ingredients, mainline.source, &mut new)
        } else {
            HashMap::default()
        };

        // Assign domains
        migrate::assignment::assign(
            &log,
            &mut mainline.ingredients,
            mainline.source,
            &new,
            &mut mainline.ndomains,
        );

        // Set up ingress and egress nodes
        let swapped1 =
            migrate::routing::add(&log, &mut mainline.ingredients, mainline.source, &mut new);

        // Merge the swap lists
        for ((dst, src), instead) in swapped1 {
            use std::collections::hash_map::Entry;
            match swapped0.entry((dst, src)) {
                Entry::Occupied(mut instead0) => {
                    if &instead != instead0.get() {
                        // This can happen if sharding decides to add a Sharder *under* a node,
                        // and routing decides to add an ingress/egress pair between that node
                        // and the Sharder. It's perfectly okay, but we should prefer the
                        // "bottommost" swap to take place (i.e., the node that is *now*
                        // closest to the dst node). This *should* be the sharding node, unless
                        // routing added an ingress *under* the Sharder. We resolve the
                        // collision by looking at which translation currently has an adge from
                        // `src`, and then picking the *other*, since that must then be node
                        // below.
                        if mainline.ingredients.find_edge(src, instead).is_some() {
                            // src -> instead -> instead0 -> [children]
                            // from [children]'s perspective, we should use instead0 for from, so
                            // we can just ignore the `instead` swap.
                        } else {
                            // src -> instead0 -> instead -> [children]
                            // from [children]'s perspective, we should use instead for src, so we
                            // need to prefer the `instead` swap.
                            *instead0.get_mut() = instead;
                        }
                    }
                }
                Entry::Vacant(hole) => {
                    hole.insert(instead);
                }
            }

            // we may also already have swapped the parents of some node *to* `src`. in
            // swapped0. we want to change that mapping as well, since lookups in swapped
            // aren't recursive.
            for (_, instead0) in swapped0.iter_mut() {
                if *instead0 == src {
                    *instead0 = instead;
                }
            }
        }
        let swapped = swapped0;

        // Find all nodes for domains that have changed
        let changed_domains: HashSet<domain::Index> = new.iter()
            .filter(|&&ni| !mainline.ingredients[ni].is_dropped())
            .map(|&ni| mainline.ingredients[ni].domain())
            .collect();
        let mut domain_nodes = mainline
            .ingredients
            .node_indices()
            .filter(|&ni| ni != mainline.source)
            .filter(|&ni| !mainline.ingredients[ni].is_dropped())
            .map(|ni| {
                (mainline.ingredients[ni].domain(), ni, new.contains(&ni))
            })
            .fold(HashMap::new(), |mut dns, (d, ni, new)| {
                dns.entry(d).or_insert_with(Vec::new).push((ni, new));
                dns
            });

        // Assign local addresses to all new nodes, and initialize them
        let mut local_remap = HashMap::new();
        let mut remap = HashMap::new();
        for (domain, nodes) in &mut domain_nodes {
            // Number of pre-existing nodes
            let mut nnodes = nodes.iter().filter(|&&(_, new)| !new).count();

            if nnodes == nodes.len() {
                // Nothing to do here
                continue;
            }

            let log = log.new(o!("domain" => domain.index()));

            // Give local addresses to every (new) node
            local_remap.clear();
            for &(ni, new) in nodes.iter() {
                if new {
                    debug!(log,
                           "assigning local index";
                           "type" => format!("{:?}", mainline.ingredients[ni]),
                           "node" => ni.index(),
                           "local" => nnodes
                    );

                    let mut ip: prelude::IndexPair = ni.into();
                    ip.set_local(unsafe { prelude::LocalNodeIndex::make(nnodes as u32) });
                    mainline.ingredients[ni].set_finalized_addr(ip);
                    local_remap.insert(ni, ip);
                    nnodes += 1;
                } else {
                    local_remap.insert(ni, *mainline.ingredients[ni].get_index());
                }
            }

            // Initialize each new node
            for &(ni, new) in nodes.iter() {
                if new && mainline.ingredients[ni].is_internal() {
                    // Figure out all the remappings that have happened
                    // NOTE: this has to be *per node*, since a shared parent may be remapped
                    // differently to different children (due to sharding for example). we just
                    // allocate it once though.
                    remap.clear();
                    remap.extend(local_remap.iter().map(|(&k, &v)| (k, v)));

                    // Parents in other domains have been swapped for ingress nodes.
                    // Those ingress nodes' indices are now local.
                    for (&(dst, src), &instead) in &swapped {
                        if dst != ni {
                            // ignore mappings for other nodes
                            continue;
                        }

                        let old = remap.insert(src, local_remap[&instead]);
                        assert_eq!(old, None);
                    }

                    trace!(log, "initializing new node"; "node" => ni.index());
                    mainline
                        .ingredients
                        .node_weight_mut(ni)
                        .unwrap()
                        .on_commit(&remap);
                }
            }
        }

        // at this point, we've hooked up the graph such that, for any given domain, the graph
        // looks like this:
        //
        //      o (egress)
        //     +.\......................
        //     :  o (ingress)
        //     :  |
        //     :  o-------------+
        //     :  |             |
        //     :  o             o
        //     :  |             |
        //     :  o (egress)    o (egress)
        //     +..|...........+.|..........
        //     :  o (ingress) : o (ingress)
        //     :  |\          :  \
        //     :  | \         :   o
        //
        // etc.
        // println!("{}", mainline);

        // Determine what nodes to materialize
        // NOTE: index will also contain the materialization information for *existing* domains
        // TODO: this should re-use materialization decisions across shard domains
        debug!(log, "calculating materializations");
        let index = domain_nodes
            .iter()
            .map(|(domain, nodes)| {
                use self::migrate::materialization::{index, pick};
                debug!(log, "picking materializations"; "domain" => domain.index());
                let mat = pick(&log, &mainline.ingredients, &nodes[..]);
                debug!(log, "deriving indices"; "domain" => domain.index());
                let idx = index(&log, &mainline.ingredients, &nodes[..], mat);
                (*domain, idx)
            })
            .collect();

        let mut uninformed_domain_nodes = domain_nodes.clone();
        let deps = migrate::transactions::analyze_graph(
            &mainline.ingredients,
            mainline.source,
            domain_nodes,
        );
        let (start_ts, end_ts, prevs) =
            mainline.checktable.perform_migration(deps.clone()).unwrap();

        info!(log, "migration claimed timestamp range"; "start" => start_ts, "end" => end_ts);

        // take snapshow of workers that are currently around; for type and lifetime reasons, we
        // have to copy the HashMap here, it seems.
        let workers = mainline.workers.clone();
        let mut placer = placement::RoundRobinPlacer::new(&workers);

        // Boot up new domains (they'll ignore all updates for now)
        debug!(log, "booting new domains");
        for domain in changed_domains {
            if mainline.domains.contains_key(&domain) {
                // this is not a new domain
                continue;
            }

            let nodes = uninformed_domain_nodes.remove(&domain).unwrap();
            let d = domain::DomainHandle::new(
                domain,
                mainline.ingredients[nodes[0].0].sharded_by(),
                &log,
                &mut mainline.ingredients,
                &mainline.readers,
                nodes,
                &mainline.persistence,
                &mainline.listen_addr,
                &mainline.checktable_addr,
                &mainline.channel_coordinator,
                &mainline.debug_channel,
                &workers,
                &mut placer,
                start_ts,
            );
            mainline.domains.insert(domain, d);
        }

        // Add any new nodes to existing domains (they'll also ignore all updates for now)
        debug!(log, "mutating existing domains");
        migrate::augmentation::inform(
            &log,
            &mut mainline,
            uninformed_domain_nodes,
            start_ts,
            prevs.unwrap(),
        );

        // Tell all base nodes about newly added columns
        for (ni, change) in self.columns {
            let n = &mainline.ingredients[ni];
            let m = match change {
                ColumnChange::Add(field, default) => box payload::Packet::AddBaseColumn {
                    node: *n.local_addr(),
                    field: field,
                    default: default,
                },
                ColumnChange::Drop(column) => box payload::Packet::DropBaseColumn {
                    node: *n.local_addr(),
                    column: column,
                },
            };

            let domain = mainline.domains.get_mut(&n.domain()).unwrap();

            domain.send(m).unwrap();
            domain.wait_for_ack().unwrap();
        }

        // Set up inter-domain connections
        // NOTE: once we do this, we are making existing domains block on new domains!
        info!(log, "bringing up inter-domain connections");
        migrate::routing::connect(&log, &mut mainline.ingredients, &mut mainline.domains, &new);

        // And now, the last piece of the puzzle -- set up materializations
        info!(log, "initializing new materializations");
        let domains_on_path = migrate::materialization::initialize(&log, mainline, &new, index);

        info!(log, "finalizing migration");

        // Ideally this should happen as part of checktable::perform_migration(), but we don't know
        // the replay paths then. It is harmless to do now since we know the new replay paths won't
        // request timestamps until after the migration in finished.
        mainline
            .checktable
            .add_replay_paths(domains_on_path)
            .unwrap();

        migrate::transactions::finalize(deps, &log, &mut mainline.domains, end_ts);

        warn!(log, "migration completed"; "ms" => dur_to_ns!(start.elapsed()) / 1_000_000);
    }
}

impl Drop for Blender {
    fn drop(&mut self) {
        for (_, d) in &mut self.domains {
            // don't unwrap, because given domain may already have terminated
            drop(d.send(box payload::Packet::Quit));
        }
        for (_, mut d) in self.domains.drain() {
            d.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Blender without any domains gets dropped once it leaves the scope.
    #[test]
    fn it_works_default() {
        // Blender gets dropped. It doesn't have Domains, so we don't see any dropped.
        let b = Blender::default();
        assert_eq!(b.ndomains, 0);
    }

    // Blender with a few domains drops them once it leaves the scope.
    #[test]
    fn it_works_blender_with_migration() {
        use Recipe;

        let r_txt = "CREATE TABLE a (x int, y int, z int);\n
                     CREATE TABLE b (r int, s int);\n";
        let mut r = Recipe::from_str(r_txt, None).unwrap();

        let mut b = Blender::new();
        {
            let mut mig = b.start_migration();
            assert!(r.activate(&mut mig, false).is_ok());
            mig.commit();
        }
    }
}
