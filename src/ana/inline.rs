//! The [`Inliner`]. The 30s summary of what this does is
//! 1. It starts from single-procedure analyses ([`regal::Body`])
//! 2. Turns them into graphs ([`ProcedureGraph`])
//! 3. Turns locations into global locations in both the graph and in the
//!    equations ([`InlinedGraph`])
//! 4. Inlines callees that are (un)marked. In the graphs the nodes are are
//!    replaced by the callee graph, reconnecting incoming and outgoing edges at
//!    the boundary. Callee locations are relativized ([`GliAt::relativize`]).
//!    Callee equations also have the bases rewritten
//!    ([`Inliner::relativize_eqs`]) before being added to the caller equations.
//! 5. Use the equations from place analysis prune edges
//!    ([`InlinedGraph::prune_impossible_edges`]) using the accumulated set of
//!    equations

use std::fmt::Write;

use flowistry::{cached::Cache, mir::borrowck_facts};
use petgraph::{
    prelude as pg,
    visit::{EdgeRef, IntoNodeReferences},
};

use crate::{
    ana::algebra::{self, Term},
    hir::BodyId,
    ir::{
        flows::CallOnlyFlow,
        global_location::IsGlobalLocation,
        regal::{self, SimpleLocation},
        GliAt, GlobalLocation, GLI,
    },
    mir,
    mir::Location,
    rust::hir::def_id::{DefId, LocalDefId},
    ty,
    utils::{
        body_name_pls, dump_file_pls, time, write_sep, AsFnAndArgs, DfppBodyExt, DisplayViaDebug,
        IntoLocalDefId, Print, RecursionBreakingCache, TinyBitSet, IntoDefId,
    },
    AnalysisCtrl, DbgArgs, Either, HashMap, HashSet, Symbol, TyCtxt,
};

/// This essentially describes a closure that determines for a given
/// [`LocalDefId`] if it should be inlined. Originally this was in fact done by
/// passing a closure, but it couldn't properly satisfy the type checker,
/// because the selector has to be stored in `fluid_let` variable, which is a
/// dynamically scoped variable. This means that the type needs to be valid for
/// a static lifetime, which I believe closures are not.
///
/// In particular the way that this works is that values of this interface are
/// then wrapped with [`RecurseSelector`], which is a flowistry interface that
/// satisfies [`flowistry::extensions::RecurseSelector`]. The wrapper then
/// simply dispatches to the [`InlineSelector`].
///
/// The reason for the two tiers of selectors is that
///
/// - Flowistry is a separate crate and so I wanted a way to control it that
///   decouples from the specifics of dfpp
/// - We use the selectors to skip functions with annotations, but I wanted to
///   keep the construction of inlined flow graphs agnostic to any notion of
///   annotations. Those are handled by the [`CollectingVisitor`]
///
/// The only implementation currently in use for this is
/// [`SkipAnnotatedFunctionSelector`].
pub trait Oracle<'tcx, 's> {
    fn should_inline(&self, did: LocalDefId) -> bool;
    fn is_semantically_meaningful(&self, did: DefId) -> bool;
}

impl<'tcx, 's, T: Oracle<'tcx, 's>> Oracle<'tcx, 's> for std::rc::Rc<T> {
    fn should_inline(&self, did: LocalDefId) -> bool {
        self.as_ref().should_inline(did)
    }
    fn is_semantically_meaningful(&self, did: DefId) -> bool {
        self.as_ref().is_semantically_meaningful(did)
    }
}

type ArgNum = u32;

type Node<C> = regal::SimpleLocation<C>;

impl<C> Node<C> {
    fn map_call<C0, F: FnOnce(&C) -> C0>(&self, f: F) -> Node<C0> {
        match self {
            Node::Return => Node::Return,
            Node::Argument(a) => Node::Argument(*a),
            Node::Call(c) => Node::Call(f(c)),
        }
    }
}

#[derive(Clone, Eq, PartialEq, Hash, Copy, serde::Serialize, serde::Deserialize)]
pub struct Edge {
    data: TinyBitSet,
    control: bool,
}

impl Edge {
    pub fn is_empty(self) -> bool {
        !self.control && self.data.is_empty()
    }
    pub fn add_type(&mut self, t: EdgeType) {
        match t {
            EdgeType::Control => self.control = true,
            EdgeType::Data(i) => self.data.set(i),
        }
    }
    pub fn empty() -> Self {
        Self {
            control: false,
            data: TinyBitSet::new_empty(),
        }
    }
    pub fn merge(&mut self, other: Self) {
        self.control |= other.control;
        self.data |= other.data;
    }
    pub fn into_types_iter(self) -> impl Iterator<Item = EdgeType> {
        self.data
            .into_iter_set_in_domain()
            .map(EdgeType::Data)
            .chain(self.control.then_some(EdgeType::Control))
    }
    pub fn count(self) -> u32 {
        self.data.count() + if self.control { 1 } else { 0 }
    }
    pub fn remove_type(&mut self, t: EdgeType) -> bool {
        let changed;
        match t {
            EdgeType::Control => {
                changed = self.control;
                self.control = false;
            }
            EdgeType::Data(i) => {
                changed = self.data.is_set(i);
                self.data.clear(i);
            }
        }
        changed
    }
    pub fn is_data(self) -> bool {
        !self.data.is_empty()
    }
    pub fn is_control(self) -> bool {
        self.control
    }
}

#[derive(Hash, Eq, PartialEq, Clone, Copy)]
pub enum EdgeType {
    Data(u32),
    Control,
}

impl std::fmt::Display for Edge {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write_sep(
            f,
            ", ",
            self.data
                .into_iter_set_in_domain()
                .map(Either::Left)
                .chain(self.control.then_some(Either::Right(()))),
            |elem, f| match elem {
                Either::Left(i) => write!(f, "arg{i}"),
                Either::Right(()) => write!(f, "ctrl"),
            },
        )
    }
}

#[derive(Clone, Copy, Eq, PartialEq, PartialOrd, Ord, Debug, Hash)]
pub struct GlobalLocal<'g> {
    local: mir::Local,
    location: Option<GlobalLocation<'g>>,
}

impl<'g> GlobalLocal<'g> {
    pub fn at_root(local: mir::Local) -> Self {
        Self {
            local,
            location: None,
        }
    }

    pub fn relative(local: mir::Local, location: GlobalLocation<'g>) -> Self {
        Self {
            local,
            location: Some(location),
        }
    }
}

impl std::fmt::Display for GlobalLocal<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?} @ ", self.local)?;
        if let Some(loc) = self.location {
            write!(f, "{}", loc)
        } else {
            f.write_str("root")
        }
    }
}

/// Common, parameterized equation type used by the [`GraphResolver`]s
pub type Equation<L> = algebra::Equality<L, DisplayViaDebug<mir::Field>>;
pub type Equations<L> = Vec<Equation<L>>;
/// Common, parameterized graph type used in this module
pub type GraphImpl<L> = pg::GraphMap<Node<(L, DefId)>, Edge, pg::Directed>;

pub struct InlinedGraph<'g> {
    graph: GraphImpl<GlobalLocation<'g>>,
    equations: Equations<GlobalLocal<'g>>,
    num_inlined: usize,
    max_call_stack_depth: usize,
}

impl<'g> InlinedGraph<'g> {
    pub fn graph(&self) -> &GraphImpl<GlobalLocation<'g>> {
        &self.graph
    }

    pub fn vertex_count(&self) -> usize {
        self.graph.node_count()
    }

    pub fn edge_count(&self) -> usize {
        self.graph
            .all_edges()
            .map(|(_, _, w)| w.count() as usize)
            .sum()
    }

    pub fn inlined_functions_count(&self) -> usize {
        self.num_inlined
    }

    pub fn max_call_stack_depth(&self) -> usize {
        self.max_call_stack_depth
    }
}

type EdgeSet<'g> = HashSet<(
    Node<(GlobalLocation<'g>, DefId)>,
    Node<(GlobalLocation<'g>, DefId)>,
)>;

impl<'tcx, 'g, 's> Inliner<'tcx, 'g, 's> {
    fn edge_has_been_pruned_before(
        from: Node<(GlobalLocation<'g>, DefId)>,
        to: Node<(GlobalLocation<'g>, DefId)>,
    ) -> bool {
        match (to, from) {
            (SimpleLocation::Call(c1), SimpleLocation::Call(c2)) => {
                c1.0.outermost() == c2.0.outermost()
            }
            (SimpleLocation::Call(c), _) | (_, SimpleLocation::Call(c)) => c.0.is_at_root(),
            _ => true,
        }
    }

    fn find_prunable_edges(
        graph: &InlinedGraph<'g>,
    ) -> HashSet<(
        Node<(GlobalLocation<'g>, DefId)>,
        Node<(GlobalLocation<'g>, DefId)>,
    )> {
        let graph = &graph.graph;
        graph
            .all_edges()
            .filter_map(|(from, to, _)| {
                (!Inliner::edge_has_been_pruned_before(from, to)).then(|| (from, to))
            })
            .collect()
    }

    /// For each edge in this graph query the set of equations to determine if
    /// it is memory-plausible e.g. if there exists an argument `a` to the
    /// target and a return `r` from the source such that either `a` can be
    /// reached from `r` or `r` can be reached from `a`.
    ///
    /// The simples example is where `r == a` a more complex example could be
    /// that `r = *a.foo`.
    fn prune_impossible_edges(
        &self,
        graph: &mut InlinedGraph<'g>,
        name: Symbol,
        edges_to_prune: &EdgeSet<'g>,
    ) {
        time(&format!("Edge Pruning for {name}"), || {
            let InlinedGraph {
                graph, equations, ..
            } = graph;
            info!("Have {} equations for pruning", equations.len());
            // debug!(
            //     "Equations for pruning are:\n{}",
            //     crate::utils::Print(|f: &mut std::fmt::Formatter<'_>| {
            //         for eq in equations.iter() {
            //             writeln!(f, "  {eq}")?;
            //         }
            //         Ok(())
            //     })
            // );
            // false.then(|| {
            //     let solver =
            //         algebra::MemoizedSolution::construct(self.equations.iter().map(|e| e.clone()));
            //     algebra::dump_dot_graph(
            //         &mut outfile_pls(&format!("{name}.eqgraph.gv")).unwrap(),
            //         &solver,
            //     )
            //     .unwrap();
            //     solver
            // });
            info!(
                "Pruning over {} edges",
                edges_to_prune
                    .into_iter()
                    .filter_map(|&(a, b)| Some(graph.edge_weight(a, b)?.count()))
                    .count()
            );

            for &(from, to) in edges_to_prune {
                if let Some(weight) = graph.edge_weight_mut(from, to) {
                    for idx in weight.data.into_iter_set_in_domain() {
                        let to_target = self.node_to_local(&to, idx);
                        // This can be optimized (directly create function)
                        let targets = match from {
                            Node::Argument(a) => Either::Right(std::iter::once(
                                GlobalLocal::at_root((a.as_usize() + 1).into()),
                            )),
                            Node::Return => unreachable!(),
                            Node::Call((location, did)) => Either::Left({
                                let call = self.get_call(location);
                                let parent = location.parent(self.gli);
                                call.argument_locals()
                                    .chain(call.return_to.into_iter())
                                    .map(move |local| {
                                        if let Some(parent) = parent {
                                            GlobalLocal::relative(local, parent)
                                        } else {
                                            GlobalLocal::at_root(local)
                                        }
                                    })
                            }),
                        }
                        .collect::<HashSet<_>>();
                        debug!(
                            "Solving for {to_target} to {}",
                            Print(|f| {
                                f.write_char('{')?;
                                write_sep(f, ", ", &targets, std::fmt::Display::fmt)?;
                                f.write_char('}')
                            })
                        );
                        let mut is_reachable = false;
                        algebra::solve_with(
                            &equations,
                            &to_target,
                            |to| targets.contains(to),
                            |term| {
                                debug!(
                                    "Found to be reachable via term {}",
                                    Print(|f| algebra::display_term_pieces(f, &term, &0))
                                );
                                is_reachable = true;
                                false
                            },
                        );

                        if !is_reachable {
                            debug!("Found unreproducible edge {from} -> {to} (idx {idx})");
                            weight.data.clear(idx)
                        }

                        // if !algebra::solve_reachable(&equations, &to_target, |to| {
                        //     targets.contains(to)
                        // }) {
                        //     debug!("Found unreproducible edge {from} -> {to} (idx {idx})");
                        //     weight.data.clear(idx)
                        // }
                    }
                    if weight.is_empty() {
                        graph.remove_edge(from, to);
                    }
                }
            }
        })
    }
}

impl<'g> InlinedGraph<'g> {
    fn from_body(
        gli: GLI<'g>,
        body_id: BodyId,
        body: &regal::Body<DisplayViaDebug<Location>>,
    ) -> Self {
        time("Graph Construction From Regal Body", || {
            debug!(
                "Equations for body are:\n{}",
                crate::utils::Print(|f: &mut std::fmt::Formatter<'_>| {
                    for eq in body.equations.iter() {
                        writeln!(f, "  {eq}")?;
                    }
                    Ok(())
                })
            );
            let equations = to_global_equations(&body.equations, body_id, gli);
            let mut gwr = InlinedGraph {
                equations,
                graph: Default::default(),
                num_inlined: 0,
                max_call_stack_depth: 0,
            };
            let g = &mut gwr.graph;
            let call_map = body
                .calls
                .iter()
                .map(|(loc, call)| (loc, call.function))
                .collect::<HashMap<_, _>>();
            // let node_map = body
            //     .calls
            //     .iter()
            //     .map(|(loc, call)| (*loc, g.add_node(Node::Call((*loc, call.function)))))
            //     .collect::<HashMap<_, _>>();
            // let arg_map = IndexVec::from_raw(
            //     body.return_arg_deps
            //         .iter()
            //         .enumerate()
            //         .map(|(i, _)| g.add_node(SimpleLocation::Argument(ArgNum::from_usize(i))))
            //         .collect(),
            // );

            let return_node = g.add_node(Node::Return);

            let mut add_dep_edges =
                |to, idx, deps: &HashSet<regal::Target<DisplayViaDebug<Location>>>| {
                    for d in deps {
                        use regal::Target;
                        let from = match d {
                            Target::Call(c) => regal::SimpleLocation::Call((
                                gli.globalize_location(**c, body_id),
                                call_map[c],
                            )),
                            Target::Argument(a) => regal::SimpleLocation::Argument(*a),
                        };
                        if g.contains_edge(from, to) {
                            g[(from, to)].add_type(idx)
                        } else {
                            let mut edge = Edge::empty();
                            edge.add_type(idx);
                            g.add_edge(from, to, edge);
                        }
                    }
                };

            for (&loc, call) in body.calls.iter() {
                let n =
                    SimpleLocation::Call((gli.globalize_location(*loc, body_id), call.function));
                for (idx, deps) in call.arguments.iter().enumerate() {
                    if let Some((_, deps)) = deps {
                        add_dep_edges(n, EdgeType::Data(idx as u32), deps)
                    }
                }
                add_dep_edges(n, EdgeType::Control, &call.ctrl_deps);
            }
            add_dep_edges(return_node, EdgeType::Data(0 as u32), &body.return_deps);
            for (idx, deps) in body.return_arg_deps.iter().enumerate() {
                add_dep_edges(
                    SimpleLocation::Argument(idx.into()),
                    EdgeType::Data(0_u32),
                    deps,
                );
            }

            gwr
        })
    }
}

type BodyCache = Cache<BodyId, regal::Body<DisplayViaDebug<Location>>>;

/// Essentially just a bunch of caches of analyses.
pub struct Inliner<'tcx, 'g, 's> {
    /// Memoized graphs created from single-procedure analyses
    base_memo: BodyCache,
    /// Memoized graphs that have all their callees inlined. Unlike `base_memo`
    /// this has to be recursion breaking, since a function may call itself
    /// (possibly transitively).
    inline_memo: RecursionBreakingCache<BodyId, InlinedGraph<'g>>,
    /// Makes choices base on labels
    oracle: &'s dyn Oracle<'tcx, 's>,
    tcx: TyCtxt<'tcx>,
    gli: GLI<'g>,
    ana_ctrl: &'static AnalysisCtrl,
    dbg_ctrl: &'static DbgArgs,
}

/// Globalize all locations mentioned in these equations.
fn to_global_equations<'g>(
    eqs: &Equations<DisplayViaDebug<mir::Local>>,
    body_id: BodyId,
    gli: GLI<'g>,
) -> Equations<GlobalLocal<'g>> {
    eqs.iter()
        .map(|eq| {
            eq.map_bases(|target| GlobalLocal {
                local: **target,
                location: None,
            })
        })
        .collect()
}

fn arg_num_to_local(a: regal::ArgumentIndex) -> mir::Local {
    (a.as_usize() + 1).into()
}

pub fn add_weighted_edge<N: petgraph::graphmap::NodeTrait, D: petgraph::EdgeType>(
    g: &mut pg::GraphMap<N, Edge, D>,
    source: N,
    target: N,
    weight: Edge,
) {
    if let Some(prior_weight) = g.edge_weight_mut(source, target) {
        prior_weight.merge(weight)
    } else {
        g.add_edge(source, target, weight);
    }
}

/// Check that this edge is not "significant" in this graph as defined by the
/// policy provided.
///
/// See what significance means in the documentation of
/// [`InconsequentialCallRemovalPolicy`](crate::args::InconsequentialCallRemovalPolicy)
fn no_significant_edge<N: petgraph::graphmap::NodeTrait>(
    g: &pg::GraphMap<N, Edge, pg::Directed>,
    n: N,
    policy: crate::args::InconsequentialCallRemovalPolicy,
) -> bool {
    // XXX verify that if the incoming edge is ctrl,
    // it's still safe to remove
    if !g
        .edges_directed(n, pg::Direction::Incoming)
        .filter(|(_, _, e)| !e.data.is_empty())
        .next()
        .is_some()
    {
        return false;
    }
    let mut has_data = false;
    for (_, _, e) in g.edges_directed(n, pg::Direction::Outgoing) {
        if e.control && !policy.remove_ctrl_flow_source() {
            return false;
        }
        if !e.data.is_empty() || e.control {
            has_data = true;
        }
    }
    has_data
}

enum InlineAction<'tcx> {
    SimpleInline(LocalDefId),
    InlineAsyncClosure(LocalDefId, mir::Place<'tcx>),
    Drop,
}

impl<'tcx, 'g, 's> Inliner<'tcx, 'g, 's> {
    pub fn new(
        tcx: TyCtxt<'tcx>,
        gli: GLI<'g>,
        recurse_selector: &'s dyn Oracle<'tcx, 's>,
        ana_ctrl: &'static AnalysisCtrl,
        dbg_ctrl: &'static DbgArgs,
    ) -> Self {
        Self {
            tcx,
            gli,
            oracle: recurse_selector,
            base_memo: Default::default(),
            inline_memo: Default::default(),
            ana_ctrl,
            dbg_ctrl,
        }
    }

    /// How many (unique) functions we have analyzed
    pub fn cache_size(&self) -> usize {
        self.inline_memo.size()
    }

    /// Compute a procedure graph for this `body_id` (memoized). Actual
    /// computation performed by [`regal::compute_from_body_id`] and
    /// [`ProcedureGraph::from`]
    fn get_procedure_graph(&self, body_id: BodyId) -> &regal::Body<DisplayViaDebug<Location>> {
        self.base_memo.get(body_id, |bid| {
            regal::compute_from_body_id(self.dbg_ctrl, bid, self.tcx, self.gli)
        })
    }

    /// Compute an inlined graph for this `body_id` (memoized)
    pub fn get_inlined_graph(&self, body_id: BodyId) -> Option<&InlinedGraph<'g>> {
        self.inline_memo.get(body_id, |bid| self.inline_graph(bid))
    }

    /// Convenience wrapper around [`Self::get_inlined_graph`]
    fn get_inlined_graph_by_def_id(&self, def_id: LocalDefId) -> Option<&InlinedGraph<'g>> {
        let hir = self.tcx.hir();
		let body_id = match hir.maybe_body_owned_by(hir.local_def_id_to_hir_id(def_id)) {
			None => { 
				warn!("no body id for {:?}", self.tcx.def_path_debug_str(def_id.into_def_id(self.tcx)));
				return None;
			},
			Some(b) => b
		};
        self.get_inlined_graph(body_id)
    }

    /// Make the set of equations relative to the call site described by `gli`
    fn relativize_eqs<'a>(
        equations: &'a Equations<GlobalLocal<'g>>,
        gli: &'a GliAt<'g>,
    ) -> impl Iterator<Item = Equation<GlobalLocal<'g>>> + 'a {
        equations.iter().map(move |eq| {
            eq.map_bases(|base| GlobalLocal {
                local: base.local,
                location: Some(
                    base.location
                        .map_or_else(|| gli.as_global_location(), |prior| gli.relativize(prior)),
                ),
            })
        })
    }

    /// Get the `regal` call description for the call site at a specific location.
    fn get_call(
        &self,
        loc: GlobalLocation<'g>,
    ) -> &regal::Call<regal::Dependencies<DisplayViaDebug<mir::Location>>> {
        let body = self.get_procedure_graph(loc.innermost_function());
        &body.calls[&DisplayViaDebug(loc.innermost_location())]
    }

    /// Calculate the global local that corresponds to input index `idx` at this `node`.
    ///
    /// If the node is not a [`SimpleLocation::Call`], then the index is ignored.
    fn node_to_local(
        &self,
        node: &Node<(GlobalLocation<'g>, DefId)>,
        idx: ArgNum,
    ) -> GlobalLocal<'g> {
        match node {
            SimpleLocation::Return => GlobalLocal::at_root(mir::RETURN_PLACE),
            SimpleLocation::Argument(idx) => GlobalLocal::at_root(arg_num_to_local(*idx)),
            SimpleLocation::Call((loc, _)) => {
                let call = self.get_call(*loc);
                let pure_local = call.arguments[(idx as usize).into()]
                    .as_ref()
                    .map(|i| i.0)
                    .unwrap();
                if let Some(parent) = loc.parent(self.gli) {
                    GlobalLocal::relative(pure_local, parent)
                } else {
                    GlobalLocal::at_root(pure_local)
                }
            }
        }
    }

    fn writeable_arguments<'tc>(
        fn_sig: &ty::FnSig<'tc>,
    ) -> impl Iterator<Item = regal::ArgumentIndex> + 'tc {
        fn_sig.inputs().iter().enumerate().filter_map(|(i, t)| {
            t.is_mutable_ptr()
                .then(|| regal::ArgumentIndex::from_usize(i))
        })
    }

    fn remove_inconsequential_calls(&self, gwr: &mut InlinedGraph<'g>) {
        let remove_calls_policy = self.ana_ctrl.remove_inconsequential_calls();
        let g = &mut gwr.graph;
        for n in g
            .nodes()
            .filter(|&n| 
                // The node has to be not semantically meaningful (e.g. no
                // label), and it has to have neighbors before and after,
                // because otherwise it's likely an I/O data source or sink
                matches!(n, SimpleLocation::Call((loc, defid)) 
                    if loc.is_at_root()
                        && !self.oracle.is_semantically_meaningful(defid)
                        && Some(defid) != self.tcx.lang_items().from_generator_fn() 
                        && defid.as_local().map_or(true, |ldid| !self.oracle.should_inline(ldid))
                        && no_significant_edge(g, n, remove_calls_policy)))
            .collect::<Vec<_>>()
        {
            for from in g
                .neighbors_directed(n, pg::Direction::Incoming)
                .collect::<Vec<_>>()
            {
                for (to, weight) in g
                    .edges_directed(n, pg::Direction::Outgoing)
                    .map(|(_, to, weight)| (to, *weight))
                    .collect::<Vec<_>>()
                {
                    add_weighted_edge(g, from, to, weight)
                }
            }
            g.remove_node(n);
        }
    }

    /// Inline crate-local, non-marked called functions and return a set of
    /// newly inserted edges that cross those function boundaries to be
    /// inspected for pruning.
    ///
    /// Note that the edges in the set are not guaranteed to exist in the graph.
    fn perform_subfunction_inlining(
        &self,
        proc_g: &regal::Body<DisplayViaDebug<Location>>,
        InlinedGraph {
            graph: g,
            equations: eqs,
            num_inlined,
            max_call_stack_depth,
        }: &mut InlinedGraph<'g>,
        body_id: BodyId,
    ) -> EdgeSet<'g> {
        let recursive_analysis_enabled = self.ana_ctrl.use_recursive_analysis();
        let local_def_id = body_id.into_local_def_id(self.tcx);
        let name = body_name_pls(self.tcx, body_id).name;
        let mut queue_for_pruning = HashSet::new();
        time(&format!("Inlining subgraphs into {name}"), || {
            let targets = g
                .node_references()
                .filter_map(|(id, n)| match n {
                    Node::Call((loc, fun)) => Some((id, loc, fun)),
                    _ => None,
                })
                .filter_map(|(id, location, function)| {
                    if recursive_analysis_enabled {
                        match function.as_local() {
                            Some(local_id) if self.oracle.should_inline(local_id) => {
                                debug!("Inlining {function:?}");
                                return Some((id, *location, InlineAction::SimpleInline(local_id)));
                            }
                            _ => (),
                        }
                        if Some(*function) == self.tcx.lang_items().from_generator_fn() {
                            let body_with_facts = borrowck_facts::get_body_with_borrowck_facts(
                                self.tcx,
                                local_def_id,
                            );
                            let body = body_with_facts.simplified_body();
                            let mut args = body
                                .stmt_at_better_err(location.innermost_location())
                                .right()
                                .expect("Expected terminator")
                                .as_fn_and_args()
                                .unwrap()
                                .1;
                            let closure = args
                                .pop()
                                .expect("Expected one closure argument")
                                .expect("Expected non-const closure argument");
                            debug_assert!(args.is_empty(), "Expected only one argument");
                            debug_assert!(closure.projection.is_empty());
                            let closure_fn = if let ty::TyKind::Generator(gid, _, _) =
                                body.local_decls[closure.local].ty.kind()
                            {
                                *gid
                            } else {
                                unreachable!("Expected Generator")
                            };
                            return Some((
                                id,
                                *location,
                                InlineAction::InlineAsyncClosure(
                                    closure_fn.as_local().unwrap(),
                                    closure,
                                ),
                            ));
                        }
                        if Some(*function) == self.tcx.lang_items().future_poll_fn() && self.ana_ctrl.drop_poll() {
                            return Some((id, *location, InlineAction::Drop));
                        }
                    }
                    let local_as_global = GlobalLocal::at_root;
                    let call = self.get_call(*location);
                    debug!("Abstracting {function:?}");
                    let fn_sig = self.tcx.fn_sig(function).skip_binder();
                    let writeables = Self::writeable_arguments(&fn_sig)
                        .filter_map(|idx| call.arguments[idx].as_ref().map(|i| i.0))
                        .chain(call.return_to.into_iter())
                        .map(local_as_global)
                        .collect::<Vec<_>>();
                    let mk_term = |tp| algebra::Term::new_base(tp);
                    eqs.extend(writeables.iter().flat_map(|&write| {
                        call.argument_locals()
                            .map(local_as_global)
                            .filter(move |read| *read != write)
                            .map(move |read| {
                                algebra::Equality::new(mk_term(write).add_unknown(), mk_term(read))
                            })
                    }));
                    None
                })
                .collect::<Vec<_>>();
            for (idx, root_location, action) in targets {
                let (def_id, is_async_closure) = match action {
                    InlineAction::InlineAsyncClosure(did, c) => (did, Some(c)),
                    InlineAction::SimpleInline(did) => (did, None),
                    InlineAction::Drop => {
                        assert!(
                            matches!(idx, SimpleLocation::Call((_, did)) if Some(did) == self.tcx.lang_items().future_poll_fn())
                        );
                        let incoming_closure = g
                            .edges_directed(idx, pg::Direction::Incoming)
                            .filter_map(|(from, to, weight)| {
                                assert!(
                                    weight.data.is_set(0) ^ weight.data.is_set(1) ^ weight.control,
                                    "{from} -> {to} ({weight})"
                                );
                                weight.data.is_set(0).then_some(from)
                            })
                            .collect::<Vec<_>>();
                        let outgoing = g
                            .edges_directed(idx, pg::Direction::Outgoing)
                            .map(|(_, to, weight)| (to, weight.clone()))
                            .collect::<Vec<_>>();

                        for from in incoming_closure {
                            for (to, weight) in outgoing.iter().cloned() {
                                add_weighted_edge(g, from, to, weight)
                            }
                        }
                        let call = self.get_call(root_location);
                        if let Some(return_local) = call.return_to {
                            eqs.push(algebra::Equality::new(
                                algebra::Term::new_base(GlobalLocal::at_root(return_local)),
                                algebra::Term::new_base(GlobalLocal::at_root(
                                    call.arguments[regal::ArgumentIndex::from_usize(0)]
                                        .as_ref()
                                        .unwrap()
                                        .0,
                                )),
                            ));
                        }
                        g.remove_node(idx);
                        // extend eqs
                        // propagate edges
                        continue;
                    }
                };
                let grw_to_inline =
                    if let Some(callee_graph) = self.get_inlined_graph_by_def_id(def_id) {
                        callee_graph
                    } else {
                        // Breaking recursion. This can only happen if we are trying to
                        // inline ourself, so we simply skip.
                        continue;
                    };
                *num_inlined += 1 + grw_to_inline.inlined_functions_count();
                *max_call_stack_depth =
                    (*max_call_stack_depth).max(grw_to_inline.max_call_stack_depth() + 1);
                let num_args = if is_async_closure.is_some() {
                    1 as usize
                } else {
                    self.tcx.fn_sig(def_id).skip_binder().inputs().len()
                };
                let mut argument_map: HashMap<_, _> = (0..num_args)
                    .map(|a| (EdgeType::Data(a as u32), vec![]))
                    .chain([(EdgeType::Control, vec![])])
                    .collect();

                for e in g.edges_directed(idx, pg::Incoming) {
                    for arg_num in e.weight().into_types_iter() {
                        argument_map.get_mut(&arg_num).unwrap().push(e.source());
                    }
                }

                assert!(root_location.is_at_root());
                let call = &proc_g.calls[&DisplayViaDebug(root_location.outermost_location())];
                let gli_here = self.gli.at(root_location.outermost_location(), body_id);
                eqs.extend(
                    Self::relativize_eqs(&grw_to_inline.equations, &gli_here).chain(
                        if let Some(closure) = is_async_closure {
                            assert!(closure.projection.is_empty());
                            Either::Right(std::iter::once((
                                closure.local,
                                arg_num_to_local(0_usize.into()),
                            )))
                        } else {
                            Either::Left((0..num_args).filter_map(|a| {
                                let a = a.into();
                                let actual_param = call.arguments[a].as_ref()?.0;
                                Some((actual_param, arg_num_to_local(a)))
                            }))
                        }
                        .into_iter()
                        .chain(call.return_to.into_iter().map(|r| (r, mir::RETURN_PLACE)))
                        .map(|(actual_param, formal_param)| {
                            algebra::Equality::new(
                                Term::new_base(GlobalLocal::at_root(actual_param)),
                                Term::new_base(GlobalLocal::relative(formal_param, root_location)),
                            )
                        }),
                    ),
                );
                let to_inline = &grw_to_inline.graph;

                let mut connect_to =
                    |g: &mut GraphImpl<_>, source, target, weight: Edge, pruning_required| {
                        let mut add_edge = |source, register_for_pruning| {
                            debug!("Connecting {source} -> {target}");
                            if register_for_pruning {
                                queue_for_pruning.insert((source, target));
                            }
                            add_weighted_edge(g, source, target, weight)
                        };
                        match source {
                            Node::Call((loc, did)) => add_edge(
                                Node::Call((gli_here.relativize(loc), did)),
                                pruning_required,
                            ),
                            Node::Return => unreachable!(),
                            Node::Argument(a) => {
                                for nidx in argument_map
                                    .get(&EdgeType::Data(a.as_usize() as u32))
                                    .into_iter()
                                    .flat_map(|s| s.into_iter())
                                {
                                    add_edge(*nidx, true)
                                }
                            }
                        }
                    };

                for old in to_inline.nodes() {
                    let new = old.map_call(|(location, function)| {
                        (gli_here.relativize(*location), *function)
                    });
                    g.add_node(new);
                    debug!(
                        "Handling {old} (now {new}) {} incoming edges",
                        to_inline.edges_directed(old, pg::Incoming).count()
                    );
                    for edge in to_inline.edges_directed(old, pg::Incoming) {
                        match new {
                            Node::Call(_) => {
                                connect_to(g, edge.source(), new, *edge.weight(), false)
                            }
                            Node::Return | Node::Argument(_) => {
                                for (target, out) in g
                                    .edges_directed(idx, pg::Outgoing)
                                    .map(|e| (e.target(), e.weight().clone()))
                                    .collect::<Vec<_>>()
                                {
                                    connect_to(g, edge.source(), target, out, true);
                                }
                            }
                        }
                    }
                }
                g.remove_node(idx);
            }
        });
        queue_for_pruning
    }

    /// In spite of the name of this function it not only inlines the graph but
    /// also first creates it (with [`Self::get_procedure_graph`]) and globalize
    /// it ([`to_global_graph`]).
    fn inline_graph(&self, body_id: BodyId) -> InlinedGraph<'g> {
        let proc_g = self.get_procedure_graph(body_id);
        let mut gwr = InlinedGraph::from_body(self.gli, body_id, proc_g);

        let name = body_name_pls(self.tcx, body_id).name;
        if self.dbg_ctrl.dump_pre_inline_graph() {
            dump_dot_graph(
                dump_file_pls(self.tcx, body_id, "pre-inline.gv").unwrap(),
                &gwr,
            )
            .unwrap();
        }
        if self.dbg_ctrl.dump_local_equations() {
            let mut eqout = dump_file_pls(self.tcx, body_id, "local.eqs").unwrap();
            for eq in &gwr.equations {
                use std::io::Write;
                writeln!(eqout, "{eq}").unwrap();
            }
        }

        let mut queue_for_pruning = self.perform_subfunction_inlining(&proc_g, &mut gwr, body_id);

        if self.ana_ctrl.remove_inconsequential_calls().is_enabled() {
            self.remove_inconsequential_calls(&mut gwr);
        }

        if self.dbg_ctrl.dump_global_equations() {
            let mut eqout = dump_file_pls(self.tcx, body_id, "global.eqs").unwrap();
            for eq in &gwr.equations {
                use std::io::Write;
                writeln!(eqout, "{eq}").unwrap();
            }
        }
        if self.dbg_ctrl.dump_inlined_graph() {
            dump_dot_graph(
                dump_file_pls(self.tcx, body_id, "inlined.gv").unwrap(),
                &gwr,
            )
            .unwrap();
        }
        if self.ana_ctrl.use_pruning() {
            let strategy = self.ana_ctrl.pruning_strategy();
            use crate::args::PruningStrategy;
            let edges_to_prune = if matches!(strategy, PruningStrategy::NotPreviouslyPrunedEdges) {
                Self::find_prunable_edges(&gwr)
            } else {
                if matches!(strategy, PruningStrategy::NewEdgesNotPreviouslyPruned) {
                    queue_for_pruning
                        .retain(|&(from, to)| !Self::edge_has_been_pruned_before(from, to));
                } else {
                    assert_eq!(strategy, PruningStrategy::NewEdges);
                }
                queue_for_pruning
            };
            self.prune_impossible_edges(&mut gwr, name, &edges_to_prune);
            if self.dbg_ctrl.dump_inlined_pruned_graph() {
                dump_dot_graph(
                    dump_file_pls(self.tcx, body_id, "inlined-pruned.gv").unwrap(),
                    &gwr,
                )
                .unwrap();
            }
        }
        gwr
    }
}

fn dump_dot_graph<W: std::io::Write>(mut w: W, g: &InlinedGraph) -> std::io::Result<()> {
    use petgraph::dot::*;
    write!(
        w,
        "{}",
        Dot::with_attr_getters(&g.graph, &[], &|_, _| "".to_string(), &|_, _| "shape=box"
            .to_string(),)
    )
}

impl<'tcx, 'g, 's> Inliner<'tcx, 'g, 's> {
    /// Turn the output of the inliner into the format the rest of the dfpp pipeline
    /// understands.
    pub fn to_call_only_flow<A: FnMut(ArgNum) -> GlobalLocation<'g>>(
        &self,
        InlinedGraph { graph: g, .. }: &InlinedGraph<'g>,
        mut mk_arg: A,
    ) -> crate::ir::CallOnlyFlow<GlobalLocation<'g>> {
        let mut location_dependencies = HashMap::new();
        let mut return_dependencies = HashSet::new();

        let mut get_dependencies = |n| {
            let mut input_deps = vec![];
            let mut ctrl_deps = HashSet::new();
            for e in g.edges_directed(n, pg::Incoming) {
                for w in e.weight().into_types_iter() {
                    let target = if let EdgeType::Data(a) = w {
                        let aidx = a as usize;
                        if aidx >= input_deps.len() {
                            input_deps.resize_with(aidx + 1, HashSet::new);
                        }
                        &mut input_deps[aidx]
                    } else {
                        &mut ctrl_deps
                    };

                    target.insert(match e.source() {
                        Node::Call(c) => c.0,
                        Node::Return => unreachable!(),
                        Node::Argument(a) => mk_arg(a.as_usize() as u32),
                    });
                }
            }
            crate::ir::CallDeps {
                input_deps,
                ctrl_deps,
            }
        };

        for (idx, n) in g.node_references() {
            match n {
                Node::Argument(_) => (),
                Node::Return => {
                    for d in get_dependencies(idx)
                        .input_deps
                        .iter()
                        .flat_map(HashSet::iter)
                    {
                        return_dependencies.insert(*d);
                    }
                }
                Node::Call((loc, _)) => assert!(location_dependencies
                    .insert(*loc, get_dependencies(idx))
                    .is_none()),
            }
        }

        CallOnlyFlow {
            location_dependencies,
            return_dependencies,
        }
    }
}