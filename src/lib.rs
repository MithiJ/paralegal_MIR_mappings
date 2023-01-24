//! Ties together the crate and defines command line options.
//!
//! While this is technically a "library", it only is so for the purposes of
//! being able to reference the same code in the two executables `dfpp` and
//! `cargo-dfpp` (a structure suggested by [rustc_plugin]).
#![feature(rustc_private)]

#[macro_use]
extern crate clap;
extern crate ordermap;
extern crate rustc_plugin;
extern crate serde;
#[macro_use]
extern crate lazy_static;
extern crate simple_logger;
#[macro_use]
extern crate log;

pub mod rust {
    //! Exposes the rustc external crates (this mod is just to tidy things up).
    pub extern crate rustc_arena;
    pub extern crate rustc_ast;
    pub extern crate rustc_borrowck;
    pub extern crate rustc_data_structures;
    pub extern crate rustc_driver;
    pub extern crate rustc_hir;
    pub extern crate rustc_index;
    pub extern crate rustc_interface;
    pub extern crate rustc_middle;
    pub extern crate rustc_mir_dataflow;
    pub extern crate rustc_query_system;
    pub extern crate rustc_span;

    pub use rustc_ast as ast;
    pub use rustc_hir as hir;
    pub use rustc_middle::mir;
    pub use rustc_middle::ty;

    pub use rustc_middle::dep_graph::DepGraph;
    pub use ty::TyCtxt;
}

use pretty::DocBuilder;
use rust::*;

use flowistry::mir::borrowck_facts;
pub use std::collections::{HashMap, HashSet};

// This import is sort of special because it comes from the private rustc
// dependencies and not from our `Cargo.toml`.
pub extern crate either;
pub use either::Either;

pub use rustc_span::Symbol;

mod ana;
pub mod ann_parse;
pub mod dbg;
pub mod desc;
pub mod frg;
pub mod ir;
mod sah;
pub mod serializers;
#[macro_use]
pub mod utils;
pub mod consts;

pub use utils::outfile_pls;

use frg::ToForge;

/// A struct so we can implement [`rustc_plugin::RustcPlugin`]
pub struct DfppPlugin;

#[derive(clap::Parser)]
struct ArgWrapper {
    /// This argument doesn't do anything, but when cargo invokes `cargo-dfpp`
    /// it always provides "dfpp" as the first argument and since we parse with
    /// clap it otherwise complains about the superfluous argument.
    _progname: String,

    /// The actual arguments
    #[clap(flatten)]
    args: Args,

    /// Pass through for additional cargo arguments (like --features)
    #[clap(last = true)]
    cargo_args: Vec<String>,
}

/// Top level command line arguments
#[derive(serde::Serialize, serde::Deserialize, clap::Args)]
pub struct Args {
    /// Print additional logging output (up to the "info" level)
    #[clap(short, long, env = "DFPP_VERBOSE")]
    verbose: bool,
    /// Print additional logging output (up to the "debug" level)
    #[clap(long, env = "DFPP_DEBUG")]
    debug: bool,
    /// Where to write the resulting forge code to (defaults to `analysis_result.frg`)
    #[clap(long, default_value = "analysis_result.frg")]
    result_path: std::path::PathBuf,
    /// Additional arguments that control the flow analysis specifically
    #[clap(flatten, next_help_heading = "Flow Analysis")]
    anactrl: AnalysisCtrl,
    /// Additional arguments that control the generation and composition of the model
    #[clap(flatten, next_help_heading = "Model Generation")]
    modelctrl: ModelCtrl,
    /// Additional arguments that control debug args specifically
    #[clap(flatten, next_help_heading = "Debugging and Testing")]
    dbg: DbgArgs,
}

#[derive(serde::Serialize, serde::Deserialize, clap::Args)]
struct ModelCtrl {
    /// A JSON file from which to load additional annotations. Whereas normally
    /// annotation can only be placed on crate-local items, these can also be
    /// placed on third party items, such as functions from the stdlib.
    ///
    /// The file is expected to contain a `HashMap<Identifier, (Vec<Annotation>,
    /// ObjectType)>`, which is the same type as `annotations` field from the
    /// `ProgramDescription` struct. It uses the `serde` derived serializer. An
    /// example for the format can be generated by running dfpp with
    /// `dump_serialized_flow_graph`.
    #[clap(long, env)]
    external_annotations: Option<std::path::PathBuf>,
}

/// Arguments that control the flow analysis
#[derive(serde::Serialize, serde::Deserialize, clap::Args)]
struct AnalysisCtrl {
    /// Disables all recursive analysis (both dfpps inlining as well as
    /// Flowistry's recursive analysis)
    #[clap(long, env)]
    no_recursive_analysis: bool,
    /// Make flowistry use a recursive analysis strategy. We turn this off by
    /// default, because we perform the recursion by ourselves and doing it
    /// twice has lead to bugs.
    #[clap(long, env)]
    recursive_flowistry: bool,
}

/// Arguments that control the output of debug information or output to be
/// consumed for testing.
#[derive(serde::Serialize, serde::Deserialize, clap::Args)]
struct DbgArgs {
    /// Dumps a table representing retrieved Flowistry matrices to stdout.
    #[clap(long, env)]
    dump_flowistry_matrix: bool,
    /// Dumps a dot graph representation of the finely granular, inlined flow of each controller.
    /// Unlike `dump_call_only_flow` this contains also statements and non-call
    /// terminators. It is also created differently (using dependency
    /// resolution) and has not been tested in a while and shouldn't be relied upon.
    #[clap(long, env)]
    dump_inlined_function_flow: bool,
    /// Dumps a dot graph representation of the dataflow between function calls
    /// calculated for each controller to <name of controller>.call-only-flow.gv
    #[clap(long, env)]
    dump_call_only_flow: bool,
    /// Deprecated alias for `dump_call_only_flow`
    #[clap(long, env)]
    dump_non_transitive_graph: bool,
    /// For each controller dumps the calculated dataflow graphs as well as
    /// information about the MIR to <name of controller>.ntgb.json. Can be
    /// deserialized with `crate::dbg::read_non_transitive_graph_and_body`.
    #[clap(long, env)]
    dump_serialized_non_transitive_graph: bool,
    /// Dump a complete `crate::desc::ProgramDescription` in serialized (json)
    /// format to "flow-graph.json". Used for testing.
    #[clap(long, env)]
    dump_serialized_flow_graph: bool,
    /// For each controller dump a dot representation for each [`mir::Body`] as
    /// provided by rustc
    #[clap(long, env)]
    dump_ctrl_mir: bool,
}

impl DbgArgs {
    fn dump_call_only_flow(&self) -> bool {
        self.dump_call_only_flow || self.dump_non_transitive_graph
    }
}

struct Callbacks {
    opts: &'static Args,
}

impl rustc_driver::Callbacks for Callbacks {
    fn config(&mut self, config: &mut rustc_interface::Config) {
        config.override_queries = Some(borrowck_facts::override_queries);
    }

    fn after_parsing<'tcx>(
        &mut self,
        _compiler: &rustc_interface::interface::Compiler,
        queries: &'tcx rustc_interface::Queries<'tcx>,
    ) -> rustc_driver::Compilation {
        let external_annotations: HashMap<_, _> =
            if let Some(annotation_file) = self.opts.modelctrl.external_annotations.as_ref() {
                serde_json::from_reader(&mut std::fs::File::open(annotation_file).unwrap()).unwrap()
            } else {
                HashMap::new()
            };
        let mut desc = queries
            .global_ctxt()
            .unwrap()
            .take()
            .enter(|tcx| ana::CollectingVisitor::new(tcx, self.opts, &external_annotations).run())
            .unwrap();
        desc.annotations.extend(external_annotations);
        if self.opts.dbg.dump_serialized_flow_graph {
            serde_json::to_writer(
                &mut std::fs::OpenOptions::new()
                    .truncate(true)
                    .create(true)
                    .write(true)
                    .open(consts::FLOW_GRAPH_OUT_NAME)
                    .unwrap(),
                &desc,
            )
            .unwrap();
        }
        info!("All elems walked");
        let mut outf = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.opts.result_path)
            .unwrap();
        let doc_alloc = pretty::BoxAllocator;
        let doc: DocBuilder<_, ()> = desc.as_forge(&doc_alloc);
        doc.render(100, &mut outf).unwrap();
        info!(
            "Wrote analysis result to {}",
            &self.opts.result_path.canonicalize().unwrap().display()
        );
        rustc_driver::Compilation::Stop
    }
}

impl rustc_plugin::RustcPlugin for DfppPlugin {
    type Args = Args;

    fn bin_name() -> String {
        "dfpp".to_string()
    }

    fn args(
        &self,
        _target_dir: &rustc_plugin::Utf8Path,
    ) -> rustc_plugin::RustcPluginArgs<Self::Args> {
        use clap::Parser;
        let args = ArgWrapper::parse();
        rustc_plugin::RustcPluginArgs {
            args: args.args,
            file: None,
            flags: None,
            cargo_args: args.cargo_args,
        }
    }

    fn run(
        self,
        compiler_args: Vec<String>,
        plugin_args: Self::Args,
    ) -> rustc_interface::interface::Result<()> {
        let lvl = if plugin_args.debug {
            log::LevelFilter::Debug
        } else if plugin_args.verbose {
            log::LevelFilter::Info
        } else {
            log::LevelFilter::Warn
        };
        simple_logger::SimpleLogger::new()
            .with_level(lvl)
            //.with_module_level("flowistry", log::LevelFilter::Error)
            .init()
            .unwrap();
        let opts = Box::leak(Box::new(plugin_args));
        rustc_driver::RunCompiler::new(&compiler_args, &mut Callbacks { opts }).run()
    }
}
