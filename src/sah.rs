/// Semantics aware hashing for MIR slices.

use crate::rust::*;
use rustc_middle::{
    hir::nested_filter::OnlyBodies,
    ty::{self, TyCtxt},
};
use crate::{HashMap, HashSet};
use crate::desc::*;
use std::cell::RefCell;

use flowistry::indexed::{impls::LocationDomain, IndexedDomain};

/// A struct that can be used to apply a `FnMut` to every `Place` in a MIR
/// object via the `visit::MutVisitor` trait. Crucial difference to
/// `PlaceVisitor` is that this function can alter the place itself.
struct RePlacer<'tcx, F>(TyCtxt<'tcx>, F);


impl<'tcx, F: FnMut(&mut mir::Place<'tcx>)> mir::visit::MutVisitor<'tcx> for RePlacer<'tcx, F> {
    fn tcx<'a>(&'a self) -> TyCtxt<'tcx> {
        self.0
    }
    fn visit_place(
        &mut self,
        place: &mut mir::Place<'tcx>,
        _context: mir::visit::PlaceContext,
        _location: mir::Location,
    ) {
        self.1(place)
    }
}

#[derive(Clone)]
struct Reindexer<I> {
    mapper: HashMap<I, I>,
    source: I,
}

impl <I> Reindexer<I> {
    fn new(base_val: I) -> Self {
        Self {
            mapper: HashMap::new(),
            source: base_val,
        }
    }

    fn reindex(&mut self, old: I) -> I 
    where 
        I: Eq + std::hash::Hash + std::ops::Add<usize, Output=I> + Copy
    {
        let ref mut src = self.source;
        *self.mapper.entry(old).or_insert_with(|| {
            let entry = *src;
            *src = entry + 1;
            entry
        })
    }
}

/// A strut with the task of renaming/repayloading all MIR values of numerical nature to create consistent code slices. Currently only `mir::Place` and basic block indices, e.g. `mir::BasicBlock` are renamed.
#[derive(Clone)]
struct MirReindexer<'tcx> {
    local_reindexer: Reindexer<mir::Local>,
    bb_reindexer: Reindexer<mir::BasicBlock>,
    tcx: TyCtxt<'tcx>,
}

impl <'tcx> MirReindexer<'tcx> {
    fn new(tcx: TyCtxt<'tcx>) -> Self {
        MirReindexer {
            tcx,
            local_reindexer: Reindexer::new(mir::Local::from_usize(0)),
            bb_reindexer: Reindexer::new(mir::BasicBlock::from_usize(0)),
        }
    }
}

impl <'tcx> mir::visit::MutVisitor<'tcx> for MirReindexer<'tcx> {
    fn tcx<'a>(&'a self) -> TyCtxt<'tcx> {
        self.tcx
    }
    fn visit_place(
        &mut self,
        place: &mut mir::Place<'tcx>,
        _context: mir::visit::PlaceContext,
        _location: mir::Location,
    ) {
        place.local = self.local_reindexer.reindex(place.local);
    }
    fn visit_terminator(&mut self,
        terminator: &mut mir::Terminator<'tcx>,
        location: mir::Location,
    ) {
        terminator.successors_mut().for_each(|suc| *suc = self.bb_reindexer.reindex(*suc));
        terminator.unwind_mut().and_then(Option::as_mut).map(|s| *s = self.bb_reindexer.reindex(*s));
        self.super_terminator(terminator, location);
    }
}

use crate::rustc_data_structures::stable_hasher::{HashStable, StableHasher};
use rustc_query_system::ich::StableHashingContext;

fn order_independent_hash<CTX, T: HashStable<CTX>,I: Iterator<Item=T>>(mut i: I, hctx: &mut CTX, hasher: &mut StableHasher) {
    let n = i.map(|it| {
        let mut hasher = StableHasher::new();
        it.hash_stable(hctx, &mut hasher);
        hasher.finish::<u128>()
    }).reduce(|one, two| one.wrapping_add(two)).unwrap_or(0);
    n.hash_stable(hctx, hasher);
}

const DUMP_VERIFICATION_HASHES : bool = false;

pub fn compute_verification_hash_for_stmt<'tcx>(
    tcx: TyCtxt<'tcx>,
    t: &mir::Terminator<'tcx>,
    loc: mir::Location,
    body: &mir::Body<'tcx>,
    loc_dom: &LocationDomain,
    matrix: &<flowistry::infoflow::FlowAnalysis<'_, 'tcx> as rustc_mir_dataflow::AnalysisDomain<
        'tcx,
    >>::Domain,
) -> VerificationHash {
    use mir::visit::Visitor;
    let mut hasher = StableHasher::new();
    use std::io::Write;
    let mut out : Box<dyn Write> = if DUMP_VERIFICATION_HASHES {
        Box::new(std::fs::OpenOptions::new().write(true).create(true).truncate(true).open("verification_hash_input").unwrap())
    } else {
        Box::new(std::io::sink())
    };
    tcx.create_stable_hashing_context().while_hashing_spans(false, |mut hctx|{

        use mir::visit::MutVisitor;
        let mut loc_set = HashSet::<mir::Location>::new();
        let mut vis = crate::ana::PlaceVisitor(|pl: &mir::Place<'tcx>| {
            loc_set.extend(matrix.row(*pl).filter(|l| crate::ana::is_real_location(loc_dom, body, **l)));
        });

        vis.visit_terminator(t, loc);
        let mut replacer = MirReindexer::new(tcx);

        // This computation does two things at one. It takes all basic
        // blocks, creating a slice over each with respect to the program
        // location of interest. Then it immediately hashes all interesting
        // locations. As a result it never needs to materialize the actual
        // slice but only its hash.
        //
        // Read the Notion to know more about this https://www.notion.so/justus-adam/Attestation-Hash-shouldn-t-change-if-unrelated-statements-change-19f4b036d85643b4ae6a9b8358e3cb70#7c35960849524580b720d3207c70004f
        type Slices<'a, 'tcx> = LazyTree<'a, mir::BasicBlock, SliceResult<'tcx>>;
        let slice_maker = |bbidx: &mir::BasicBlock, tree: &mut Slices<'_, 'tcx>| {
            let bbdat = &body.basic_blocks()[*bbidx]; 
            let termidx = bbdat.statements.len();
            let mut new_stmts : Vec<mir::Statement> = bbdat.statements.iter().enumerate().filter_map(|(stmtidx, stmt)| {
                let loc = mir::Location {block: *bbidx, statement_index: stmtidx};
                if loc_set.contains(&loc) {
                    Some(stmt.clone())
                } else {
                    None
                }
            }).collect();
            let termloc = mir::Location {block: *bbidx, statement_index: termidx};
            if loc_set.contains(&termloc) {
                let mut renamer_may = None;
                let mut dom = *bbidx;
                while renamer_may.is_none() {
                    dom = body.dominators().immediate_dominator(dom);
                    renamer_may = if dom == *bbidx {
                        Some(MirReindexer::new(tcx))
                    } else {
                        match tree.get(&dom) {
                            SliceResult::Dropped => None,
                            SliceResult::Sliced{ renamer, .. } => Some(renamer.clone()),
                            SliceResult::Consolidate(stmts) => {
                                assert!(!stmts.is_empty());
                                new_stmts = stmts.drain(..).chain(new_stmts.into_iter()).collect();
                                None
                            }
                        }
                    };
                }
                let mut renamer = renamer_may.unwrap();
                let mut new_block = mir::BasicBlockData {
                    statements: new_stmts,
                    terminator: bbdat.terminator.clone(),
                    is_cleanup: bbdat.is_cleanup,
                };
                renamer.visit_basic_block_data(*bbidx, &mut new_block);
                SliceResult::Sliced{ slice: new_block, renamer }
            } else if !new_stmts.is_empty() {
                SliceResult::Consolidate(new_stmts)
            } else {
                SliceResult::Dropped
            }
        };
        
        let mut slice : RefCell<Slices> = RefCell::new(LazyTree::new(&slice_maker));
        let hctx_cell = RefCell::new(hctx);
        type Hashes<'a, 'tcx> = LazyTree<'a, mir::BasicBlock, VerificationHash>;
        let hashes_maker = |bb : &mir::BasicBlock, tree: &mut Hashes<'_, 'tcx>| {
            let mut hasher = StableHasher::new();
            let subs = 
                (&body.basic_blocks()[*bb]).terminator().successors().filter(|b| b != bb).filter_map(|b| /* This is a bit dangerous. I've added this filter because it is possible you are transitively in your own successors. However there could be other things wrong with the code that could also cause that situation which this would ignore. */  tree.get_may(&b).map(|b| *b)).collect::<Vec<_>>();
            order_independent_hash(
                subs.into_iter(),
                &mut hctx_cell.borrow_mut(),
                &mut hasher,
            );
            slice.borrow_mut().get(bb).slice_may().map(|b| b.hash_stable(&mut hctx_cell.borrow_mut(), &mut hasher));
            hasher.finish()
        };
        let mut hashes : Hashes<'_, 'tcx> = LazyTree::new(&hashes_maker);

        hashes.get(&mir::START_BLOCK).hash_stable(&mut hctx_cell.borrow_mut(), &mut hasher);

    });
    hasher.finish()
}

enum SliceResult<'tcx> {
    Dropped,
    Sliced {
        slice: mir::BasicBlockData<'tcx>,
        renamer: MirReindexer<'tcx>,
    },
    Consolidate(Vec<mir::Statement<'tcx>>),
}

impl <'tcx> SliceResult<'tcx> {
    fn slice_may(&self) -> Option<&mir::BasicBlockData<'tcx>> {
        match self {
            SliceResult::Sliced {slice, .. } => Some(slice),
            _ => None
        }
    }
}

/// This structure allows you to use knot-tying to compute values that depend on
/// each other. The laziness is quite convenient, because it means you can focus
/// on just one element at a time and assume that all others have already been
/// computed. This is done by invoking `get` in the `make` function to recurse.
/// However there is a caveat, you have to ensure that you traverse this
/// hierarchy in only one direction at a time or that you early initialize a
/// value (using `init`), otherwise you will create an infinite loop.
struct LazyTree<'a, I, P> {
    tree: HashMap<I, Option<P>>,
    make: &'a dyn Fn(&I, &mut Self) -> P,
}

impl <'a, I: Eq + std::hash::Hash + Clone, P> LazyTree<'a, I, P> {
    fn new<F: Fn(&I, &mut Self) -> P>(make: &'a F) -> Self {
        Self {
            tree: HashMap::new(),
            make,
        }
    }

    fn get_may<'b>(&'b mut self, i: &I) -> Option<&'b mut P> {
        if !self.tree.contains_key(&i) {
            let f = self.make;
            self.tree.insert(i.clone(), None);
            let v = f(i, self);
            assert!(self.tree.insert(i.clone(), Some(v)).as_ref().map_or(false, Option::is_none));
        };
        self.tree.get_mut(i).unwrap().as_mut()
    }

    fn get(&mut self, i: &I) -> &mut P {
        self.get_may(i).expect("Still initializing")
    }
}