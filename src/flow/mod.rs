use backlog;
use checktable;
use ops::base::Base;

use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::sync::mpsc;
use std::thread;
use std::time;
use std::fmt;
use std::io;

use slog;
use petgraph;
use petgraph::graph::NodeIndex;

pub mod domain;
pub mod prelude;
pub mod node;
pub mod payload;
pub mod statistics;
pub mod keys;
pub mod core;
pub mod migrate;
mod transactions;
mod hook;

mod mutator;
pub use self::mutator::Mutator;

const NANOS_PER_SEC: u64 = 1_000_000_000;
macro_rules! dur_to_ns {
    ($d:expr) => {{
        let d = $d;
        d.as_secs() * NANOS_PER_SEC + d.subsec_nanos() as u64
    }}
}

lazy_static! {
    static ref VIEW_READERS: Mutex<HashMap<NodeIndex, backlog::ReadHandle>> = Mutex::default();
}

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
    checktable: Arc<Mutex<checktable::CheckTable>>,
    partial: HashSet<NodeIndex>,
    partial_enabled: bool,

    txs: HashMap<domain::Index, mpsc::SyncSender<payload::Packet>>,
    in_txs: HashMap<domain::Index, mpsc::SyncSender<payload::Packet>>,
    domains: Vec<thread::JoinHandle<()>>,

    log: slog::Logger,
}

impl Default for Blender {
    fn default() -> Self {
        let mut g = petgraph::Graph::new();
        let source = g.add_node(node::Node::new("source",
                                                &["because-type-inference"],
                                                node::Type::Source,
                                                true));
        Blender {
            ingredients: g,
            source: source,
            ndomains: 0,
            checktable: Arc::new(Mutex::new(checktable::CheckTable::new())),
            partial: Default::default(),
            partial_enabled: true,

            txs: HashMap::default(),
            in_txs: HashMap::default(),
            domains: Vec::new(),

            log: slog::Logger::root(slog::Discard, o!()),
        }
    }
}

impl Blender {
    /// Construct a new, empty `Blender`
    pub fn new() -> Self {
        Blender::default()
    }

    /// Disable partial materialization for all subsequent migrations
    pub fn disable_partial(&mut self) {
        self.partial_enabled = false;
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
        let checktable = self.checktable.clone();
        Box::new(move |t: &checktable::Token| checktable.lock().unwrap().validate_token(t))
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
    pub fn inputs(&self) -> Vec<(core::NodeAddress, &node::Node)> {
        self.ingredients
            .neighbors_directed(self.source, petgraph::EdgeDirection::Outgoing)
            .flat_map(|ingress| {
                          self.ingredients
                              .neighbors_directed(ingress, petgraph::EdgeDirection::Outgoing)
                      })
            .map(|n| (n, &self.ingredients[n]))
            .filter(|&(_, base)| base.is_internal() && base.get_base().is_some())
            .map(|(n, base)| (n.into(), &*base))
            .collect()
    }

    /// Get a reference to all known output nodes.
    ///
    /// Output nodes here refers to nodes of type `Reader`, which is the nodes created in response
    /// to calling `.maintain` or `.stream` for a node during a migration.
    ///
    /// This function will only tell you which nodes are output nodes in the graph. To obtain a
    /// function for performing reads, call `.get_reader()` on the returned reader.
    pub fn outputs(&self) -> Vec<(core::NodeAddress, &node::Node)> {
        self.ingredients
            .externals(petgraph::EdgeDirection::Outgoing)
            .filter_map(|n| {
                use flow::node;
                if let node::Type::Reader(..) = *self.ingredients[n] {
                    // we want to give the the node that is being materialized
                    // not the reader node itself
                    let src = self.ingredients
                        .neighbors_directed(n, petgraph::EdgeDirection::Incoming)
                        .next()
                        .unwrap();
                    Some((src.into(), &self.ingredients[src]))
                } else {
                    None
                }
            })
            .collect()
    }

    /// Obtain a new function for querying a given (already maintained) reader node.
    pub fn get_getter
        (&self,
         node: core::NodeAddress)
         -> Option<Box<Fn(&prelude::DataType, bool) -> Result<core::Datas, ()> + Send>> {

        // reader should be a child of the given node
        let reader = self.ingredients
            .neighbors_directed(*node.as_global(), petgraph::EdgeDirection::Outgoing)
            .filter(|&ni| if let node::Type::Reader(..) = *self.ingredients[ni] {
                        true
                    } else {
                        false
                    })
            .next(); // there should be at most one

        // look up the read handle, clone it, and construct the read closure
        reader.and_then(|r| {
            let vr = VIEW_READERS.lock().unwrap();
            let rh: Option<backlog::ReadHandle> = vr.get(&r).cloned();
            rh.map(|rh| {
                Box::new(move |q: &prelude::DataType,
                               block: bool|
                               -> Result<prelude::Datas, ()> {
                    rh.find_and(q,
                                  |rs| {
                            rs.into_iter()
                                .map(|v| (&**v).into_iter().map(|v| v.external_clone()).collect())
                                .collect()
                        },
                                  block)
                        .map(|r| r.0.unwrap_or_else(Vec::new))
                }) as Box<_>
            })
        })
    }

    /// Obtain a new function for querying a given (already maintained) transactional reader node.
    pub fn get_transactional_getter(&self,
                                    node: core::NodeAddress)
                                    -> Result<Box<Fn(&prelude::DataType)
                                                     -> Result<(core::Datas, checktable::Token),
                                                                ()> + Send>,
                                              ()> {

        if !self.ingredients[*node.as_global()].is_transactional() {
            return Err(());
        }

        // reader should be a child of the given node
        let reader = self.ingredients
            .neighbors_directed(*node.as_global(), petgraph::EdgeDirection::Outgoing)
            .filter_map(|ni| if let node::Type::Reader(_, ref inner) = *self.ingredients[ni] {
                            Some((ni, inner))
                        } else {
                            None
                        })
            .next(); // there should be at most one

        // look up the read handle, clone it, and construct the read closure
        reader
            .and_then(|(r, inner)| {
                let vr = VIEW_READERS.lock().unwrap();
                let rh: Option<backlog::ReadHandle> = vr.get(&r).cloned();
                rh.map(|rh| {
                    let generator = inner.token_generator.clone().unwrap();
                    Box::new(move |q: &prelude::DataType|
                                   -> Result<(core::Datas, checktable::Token), ()> {
                        rh.find_and(q,
                                      |rs| {
                                rs.into_iter()
                                    .map(|v| {
                                             (&**v)
                                                 .into_iter()
                                                 .map(|v| v.external_clone())
                                                 .collect()
                                         })
                                    .collect()
                            },
                                      true)
                            .map(|(res, ts)| {
                                     let token = generator.generate(ts, q.clone());
                                     (res.unwrap_or_else(Vec::new), token)
                                 })
                    }) as Box<_>
                })
            })
            .ok_or(())
    }

    /// Obtain a mutator that can be used to perform writes and deletes from the given base node.
    pub fn get_mutator(&self, base: core::NodeAddress) -> Mutator {
        let n = self.ingredients
            .neighbors_directed(*base.as_global(), petgraph::EdgeDirection::Incoming)
            .next()
            .unwrap();
        let node = &self.ingredients[n];
        let tx = self.in_txs[&node.domain()].clone();

        trace!(self.log, "creating mutator"; "for" => n.index());

        let base_n = self.ingredients[*base.as_global()]
            .get_base()
            .expect("asked to get mutator for non-base node");
        Mutator {
            src: self.source.into(),
            tx: tx,
            addr: node.addr(),
            primary_key: self.ingredients[*base.as_global()]
                .suggest_indexes(base)
                .remove(&base)
                .unwrap_or_else(Vec::new),
            tx_reply_channel: mpsc::channel(),
            transactional: self.ingredients[*base.as_global()].is_transactional(),
            dropped: base_n.get_dropped(),
            tracer: None,
        }
    }

    /// Get statistics about the time spent processing different parts of the graph.
    pub fn get_statistics(&mut self) -> statistics::GraphStats {
        // TODO: request stats from domains in parallel.
        let domains = self.txs
            .iter()
            .map(|(di, s)| {
                let (tx, rx) = mpsc::sync_channel(1);
                s.send(payload::Packet::GetStatistics(tx)).unwrap();

                let (domain_stats, node_stats) = rx.recv().unwrap();
                let node_map = node_stats
                    .into_iter()
                    .map(|(ni, ns)| (ni.into(), ns))
                    .collect();

                (*di, (domain_stats, node_map))
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
    added: HashMap<NodeIndex, Option<domain::Index>>,
    columns: Vec<(NodeIndex, ColumnChange)>,
    readers: HashMap<NodeIndex, NodeIndex>,
    materialize: HashSet<(NodeIndex, NodeIndex)>,

    start: time::Instant,
    log: slog::Logger,
}

impl<'a> Migration<'a> {
    /// Add a new (empty) domain to the graph
    pub fn add_domain(&mut self) -> domain::Index {
        trace!(self.log, "creating new domain"; "domain" => self.mainline.ndomains);
        self.mainline.ndomains += 1;
        (self.mainline.ndomains - 1).into()
    }

    /// Add the given `Ingredient` to the Soup.
    ///
    /// The returned identifier can later be used to refer to the added ingredient.
    /// Edges in the data flow graph are automatically added based on the ingredient's reported
    /// `ancestors`.
    pub fn add_ingredient<S1, FS, S2, I>(&mut self, name: S1, fields: FS, i: I) -> core::NodeAddress
        where S1: ToString,
              S2: ToString,
              FS: IntoIterator<Item = S2>,
              I: Into<node::Type>
    {
        let mut i = i.into();
        i.on_connected(&self.mainline.ingredients);

        let parents = i.ancestors();

        let transactional =
            !parents.is_empty() &&
            parents
                .iter()
                .all(|p| self.mainline.ingredients[*p.as_global()].is_transactional());

        // add to the graph
        let ni = self.mainline
            .ingredients
            .add_node(node::Node::new(name.to_string(), fields, i, transactional));
        info!(self.log,
              "adding new node";
              "node" => ni.index(),
              "type" => format!("{:?}", *self.mainline.ingredients[ni])
        );

        // keep track of the fact that it's new
        self.added.insert(ni, None);
        // insert it into the graph
        if parents.is_empty() {
            self.mainline
                .ingredients
                .add_edge(self.mainline.source, ni, false);
        } else {
            for parent in parents {
                self.mainline
                    .ingredients
                    .add_edge(*parent.as_global(), ni, false);
            }
        }
        // and tell the caller its id
        ni.into()
    }

    /// Add a transactional base node to the graph
    pub fn add_transactional_base<S1, FS, S2>(&mut self,
                                              name: S1,
                                              fields: FS,
                                              b: Base)
                                              -> core::NodeAddress
        where S1: ToString,
              S2: ToString,
              FS: IntoIterator<Item = S2>
    {
        let mut i: node::Type = b.into();
        i.on_connected(&self.mainline.ingredients);

        // add to the graph
        let ni = self.mainline
            .ingredients
            .add_node(node::Node::new(name.to_string(), fields, i, true));
        info!(self.log,
              "adding new node";
              "node" => ni.index(),
              "type" => format!("{:?}", *self.mainline.ingredients[ni])
        );

        // keep track of the fact that it's new
        self.added.insert(ni, None);
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
    pub fn add_column<S: ToString>(&mut self,
                                   node: core::NodeAddress,
                                   field: S,
                                   default: prelude::DataType)
                                   -> usize {
        // not allowed to add columns to new nodes
        assert!(!self.added.contains_key(node.as_global()));

        let field = field.to_string();
        let base = &mut self.mainline.ingredients[*node.as_global()];
        assert!(base.is_internal() && base.get_base().is_some());

        // we need to tell the base about its new column and its default, so that old writes that
        // do not have it get the additional value added to them.
        let col_i1 = base.add_column(&field);
        // we can't rely on DerefMut, since it disallows mutating Taken nodes
        if let &mut node::NodeHandle::Taken(ref mut base) = base.inner_mut() {
            let col_i2 = base.get_base_mut().unwrap().add_column(default.clone());
            assert_eq!(col_i1, col_i2);
        }

        // also eventually propagate to domain clone
        self.columns
            .push((*node.as_global(), ColumnChange::Add(field, default)));

        col_i1
    }

    /// Drop a column from a base node.
    pub fn drop_column(&mut self, node: core::NodeAddress, column: usize) {
        // not allowed to drop columns from new nodes
        assert!(!self.added.contains_key(node.as_global()));

        let base = &mut self.mainline.ingredients[*node.as_global()];
        assert!(base.is_internal() && base.get_base().is_some());

        // we need to tell the base about the dropped column, so that old writes that contain that
        // column will have it filled in with default values (this is done in Mutator).
        // we can't rely on DerefMut, since it disallows mutating Taken nodes
        if let &mut node::NodeHandle::Taken(ref mut base) = base.inner_mut() {
            base.get_base_mut().unwrap().drop_column(column);
        }

        // also eventually propagate to domain clone
        self.columns
            .push((*node.as_global(), ColumnChange::Drop(column)));
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
    pub fn materialize(&mut self, src: core::NodeAddress, dst: core::NodeAddress) {
        // TODO
        // what about if a user tries to materialize a cross-domain edge that has already been
        // converted to an egress/ingress pair?
        let e = self.mainline
            .ingredients
            .find_edge(*src.as_global(), *dst.as_global())
            .expect("asked to materialize non-existing edge");

        debug!(self.log, "told to materialize"; "node" => src.as_global().index());

        let mut e = self.mainline.ingredients.edge_weight_mut(e).unwrap();
        if !*e {
            *e = true;
            // it'd be nice if we could just store the EdgeIndex here, but unfortunately that's not
            // guaranteed by petgraph to be stable in the presence of edge removals (which we do in
            // commit())
            self.materialize
                .insert((*src.as_global(), *dst.as_global()));
        }
    }

    /// Assign the ingredient with identifier `n` to the thread domain `d`.
    ///
    /// `n` must be have been added in this migration.
    pub fn assign_domain(&mut self, n: core::NodeAddress, d: domain::Index) {
        // TODO: what if a node is added to an *existing* domain?
        debug!(self.log,
               "node manually assigned to domain";
               "node" => n.as_global().index(),
               "domain" => d.index()
        );
        assert_eq!(self.added.insert(*n.as_global(), Some(d)).unwrap(), None);
    }

    fn ensure_reader_for(&mut self, n: core::NodeAddress) {
        if !self.readers.contains_key(n.as_global()) {
            // make a reader
            let r = node::Type::Reader(None, Default::default());
            let r = self.mainline.ingredients[*n.as_global()].mirror(r);
            let r = self.mainline.ingredients.add_node(r);
            self.mainline.ingredients.add_edge(*n.as_global(), r, false);
            self.readers.insert(*n.as_global(), r);
        }
    }

    fn ensure_token_generator(&mut self, n: core::NodeAddress, key: usize) {
        let ri = self.readers[n.as_global()];
        if let node::Type::Reader(_, ref mut inner) = *self.mainline.ingredients[ri] {
            if inner.token_generator.is_some() {
                return;
            }
        } else {
            unreachable!("tried to add token generator to non-reader node");
        }

        let base_columns: Vec<(_, Option<_>)> =
            self.mainline.ingredients[*n.as_global()]
                .base_columns(key, &self.mainline.ingredients, *n.as_global());

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
            .lock()
            .unwrap()
            .track(&token_generator);

        if let node::Type::Reader(_, ref mut inner) = *self.mainline.ingredients[ri] {
            inner.token_generator = Some(token_generator);
        }
    }

    fn reader_for(&mut self, n: core::NodeAddress) -> &mut node::Reader {
        let ri = self.readers[n.as_global()];
        if let node::Type::Reader(_, ref mut inner) = *self.mainline.ingredients[ri] {
            &mut *inner
        } else {
            unreachable!("tried to use non-reader node as a reader")
        }
    }

    /// Set up the given node such that its output can be efficiently queried.
    ///
    /// To query into the maintained state, use `Blender::get_getter` or
    /// `Blender::get_transactional_getter`
    pub fn maintain(&mut self, n: core::NodeAddress, key: usize) {
        self.ensure_reader_for(n);
        if self.mainline.ingredients[*n.as_global()].is_transactional() {
            self.ensure_token_generator(n, key);
        }

        let ri = self.readers[n.as_global()];

        if let node::Type::Reader(_, ref mut inner) = *self.mainline.ingredients[ri] {
            if let Some(skey) = inner.state {
                assert_eq!(skey, key);
            } else {
                inner.state = Some(key);
            }
        } else {
            unreachable!("tried to use non-reader node as a reader")
        }
    }

    /// Obtain a channel that is fed by the output stream of the given node.
    ///
    /// As new updates are processed by the given node, its outputs will be streamed to the
    /// returned channel. Node that this channel is *not* bounded, and thus a receiver that is
    /// slower than the system as a hole will accumulate a large buffer over time.
    pub fn stream(&mut self, n: core::NodeAddress) -> mpsc::Receiver<Vec<node::StreamUpdate>> {
        self.ensure_reader_for(n);
        let (tx, rx) = mpsc::channel();

        // If the reader hasn't been incorporated into the graph yet, just add the streamer
        // directly.
        if let Some(ref mut streamers) = self.reader_for(n).streamers {
            streamers.push(tx);
            return rx;
        }

        // Otherwise, send a message to the reader's domain to have it add the streamer.
        let reader = &self.mainline.ingredients[self.readers[n.as_global()]];
        self.mainline.txs[&reader.domain()]
            .send(payload::Packet::AddStreamer {
                      node: reader.addr().as_local().clone(),
                      new_streamer: tx,
                  })
            .unwrap();

        rx
    }

    /// Set up the given node such that its output is stored in Memcached.
    pub fn memcached_hook(&mut self,
                          n: core::NodeAddress,
                          name: String,
                          servers: &[(&str, usize)],
                          key: usize)
                          -> io::Result<core::NodeAddress> {
        let h = try!(hook::Hook::new(name, servers, vec![key]));
        let h = node::Type::Hook(Some(h));
        let h = self.mainline.ingredients[*n.as_global()].mirror(h);
        let h = self.mainline.ingredients.add_node(h);
        self.mainline.ingredients.add_edge(*n.as_global(), h, false);
        Ok(h.into())
    }

    /// Commit the changes introduced by this `Migration` to the master `Soup`.
    ///
    /// This will spin up an execution thread for each new thread domain, and hook those new
    /// domains into the larger Soup graph. The returned map contains entry points through which
    /// new updates should be sent to introduce them into the Soup.
    pub fn commit(self) {
        info!(self.log, "finalizing migration"; "#nodes" => self.added.len());
        let mut new = HashSet::new();

        let log = self.log;
        let start = self.start;
        let mainline = self.mainline;

        // Make sure all new nodes are assigned to a domain
        for (node, domain) in self.added {
            let domain = domain.unwrap_or_else(|| {
                // new node that doesn't belong to a domain
                // create a new domain just for that node
                // NOTE: this is the same code as in add_domain(), but we can't use self here
                trace!(log,
                       "node automatically added to domain";
                       "node" => node.index(),
                       "domain" => mainline.ndomains
                );
                mainline.ndomains += 1;
                (mainline.ndomains - 1).into()

            });
            mainline.ingredients[node].add_to(domain);
            new.insert(node);
        }

        // Readers are nodes too.
        // And they should be assigned the same domain as their parents
        for (parent, reader) in self.readers {
            let domain = mainline.ingredients[parent].domain();
            mainline.ingredients[reader].add_to(domain);
            new.insert(reader);
        }

        // Set up ingress and egress nodes
        let mut swapped =
            migrate::routing::add(&log, &mut mainline.ingredients, mainline.source, &mut new);

        // Find all nodes for domains that have changed
        let changed_domains: HashSet<_> = new.iter()
            .map(|&ni| mainline.ingredients[ni].domain())
            .collect();
        let mut domain_nodes = mainline
            .ingredients
            .node_indices()
            .filter(|&ni| ni != mainline.source)
            .map(|ni| {
                     let domain = mainline.ingredients[ni].domain();
                     (domain, ni, new.contains(&ni))
                 })
            .fold(HashMap::new(), |mut dns, (d, ni, new)| {
                dns.entry(d).or_insert_with(Vec::new).push((ni, new));
                dns
            });

        let mut rxs = HashMap::new();

        // Set up input channels for new domains
        for domain in domain_nodes.keys() {
            if !mainline.txs.contains_key(domain) {
                let (in_tx, in_rx) = mpsc::sync_channel(256);
                let (tx, rx) = mpsc::sync_channel(1);
                rxs.insert(*domain, (rx, in_rx));
                mainline.txs.insert(*domain, tx);
                mainline.in_txs.insert(*domain, in_tx);
            }
        }

        // Assign local addresses to all new nodes, and initialize them
        for (domain, nodes) in &mut domain_nodes {
            // Number of pre-existing nodes
            let mut nnodes = nodes.iter().filter(|&&(_, new)| !new).count();

            if nnodes == nodes.len() {
                // Nothing to do here
                continue;
            }

            let log = log.new(o!("domain" => domain.index()));

            // Give local addresses to every (new) node
            for &(ni, new) in nodes.iter() {
                if new {
                    debug!(log,
                           "assigning local index";
                           "type" => format!("{:?}", *mainline.ingredients[ni]),
                           "node" => ni.index(),
                           "local" => nnodes
                    );
                    mainline.ingredients[ni]
                        .set_addr(unsafe { prelude::NodeAddress::make_local(nnodes as u32) });
                    nnodes += 1;
                }
            }

            // Figure out all the remappings that have happened
            let mut remap = HashMap::new();
            // The global address of each node in this domain is now a local one
            for &(ni, _) in nodes.iter() {
                remap.insert(ni.into(), mainline.ingredients[ni].addr());
            }
            // Parents in other domains have been swapped for ingress nodes.
            // Those ingress nodes' indices are now local.
            for (from, to) in swapped.remove(domain).unwrap_or_else(HashMap::new) {
                remap.insert(from.into(), mainline.ingredients[to].addr());
            }

            // Initialize each new node
            for &(ni, new) in nodes.iter() {
                if new && mainline.ingredients[ni].is_internal() {
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
        debug!(log, "calculating materializations");
        let index = domain_nodes
            .iter()
            .map(|(domain, nodes)| {
                     use self::migrate::materialization::{pick, index};
                     debug!(log, "picking materializations"; "domain" => domain.index());
                     let mat = pick(&log, &mainline.ingredients, &nodes[..]);
                     debug!(log, "deriving indices"; "domain" => domain.index());
                     let idx = index(&log, &mainline.ingredients, &nodes[..], mat);
                     (*domain, idx)
                 })
            .collect();

        let mut uninformed_domain_nodes = domain_nodes.clone();
        let ingresses_from_base = migrate::transactions::analyze_graph(&mainline.ingredients,
                                                                       mainline.source,
                                                                       domain_nodes);
        let (start_ts, end_ts, prevs) = mainline
            .checktable
            .lock()
            .unwrap()
            .perform_migration(&ingresses_from_base);

        info!(log, "migration claimed timestamp range"; "start" => start_ts, "end" => end_ts);

        // Boot up new domains (they'll ignore all updates for now)
        debug!(log, "booting new domains");
        for domain in changed_domains {
            if !rxs.contains_key(&domain) {
                // this is not a new domain
                continue;
            }

            // Start up new domain
            let (rx, in_rx) = rxs.remove(&domain).unwrap();
            let jh = migrate::booting::boot_new(log.new(o!("domain" => domain.index())),
                                                domain.index().into(),
                                                &mut mainline.ingredients,
                                                uninformed_domain_nodes.remove(&domain).unwrap(),
                                                mainline.checktable.clone(),
                                                rx,
                                                in_rx,
                                                start_ts);
            mainline.domains.push(jh);
        }
        drop(rxs);

        // Add any new nodes to existing domains (they'll also ignore all updates for now)
        debug!(log, "mutating existing domains");
        migrate::augmentation::inform(&log,
                                      &mut mainline.ingredients,
                                      mainline.source,
                                      &mut mainline.txs,
                                      uninformed_domain_nodes,
                                      start_ts,
                                      prevs.unwrap());

        // Tell all base nodes about newly added columns
        let acks: Vec<_> = self.columns
            .into_iter()
            .map(|(ni, change)| {
                let (tx, rx) = mpsc::sync_channel(1);
                let n = &mainline.ingredients[ni];
                let m = match change {
                    ColumnChange::Add(field, default) => {
                        payload::Packet::AddBaseColumn {
                            node: *n.addr().as_local(),
                            field: field,
                            default: default,
                            ack: tx,
                        }
                    }
                    ColumnChange::Drop(column) => {
                        payload::Packet::DropBaseColumn {
                            node: *n.addr().as_local(),
                            column: column,
                            ack: tx,
                        }
                    }
                };
                mainline.txs[&n.domain()].send(m).unwrap();
                rx
            })
            .collect();
        // wait for all domains to ack. otherwise, we could have one domain request a replay from
        // another before the source domain has heard about a new default column it needed to add.
        for ack in acks {
            ack.recv().is_err();
        }

        // Set up inter-domain connections
        // NOTE: once we do this, we are making existing domains block on new domains!
        info!(log, "bringing up inter-domain connections");
        migrate::routing::connect(&log, &mut mainline.ingredients, &mainline.txs, &new);

        // And now, the last piece of the puzzle -- set up materializations
        info!(log, "initializing new materializations");
        let domains_on_path = migrate::materialization::initialize(&log,
                                                                   &mut mainline.ingredients,
                                                                   mainline.source,
                                                                   &new,
                                                                   &mut mainline.partial,
                                                                   mainline.partial_enabled,
                                                                   index,
                                                                   &mut mainline.txs);

        info!(log, "finalizing migration");

        // Ideally this should happen as part of checktable::perform_migration(), but we don't know
        // the replay paths then. It is harmless to do now since we know the new replay paths won't
        // request timestamps until after the migration in finished.
        mainline
            .checktable
            .lock()
            .unwrap()
            .add_replay_paths(domains_on_path);

        migrate::transactions::finalize(ingresses_from_base, &log, &mut mainline.txs, end_ts);

        warn!(log, "migration completed"; "ms" => dur_to_ns!(start.elapsed()) / 1_000_000);
    }
}

impl Drop for Blender {
    fn drop(&mut self) {
        for (_, tx) in &mut self.txs {
            // don't unwrap, because given domain may already have terminated
            drop(tx.send(payload::Packet::Quit));
        }
        for d in self.domains.drain(..) {
            d.join().unwrap();
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

    #[test]
    fn small_packets() {
        use std::mem;
        assert!(mem::size_of::<prelude::Packet>() <= 128,
                format!("Packets are too big ({} bytes)",
                        mem::size_of::<prelude::Packet>()));
    }
}
