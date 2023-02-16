#![feature(rustc_private)]
#[macro_use]
extern crate lazy_static;

use dfpp::Symbol;
mod helpers;
use helpers::*;

fn do_in_crate_dir<A, F: std::panic::UnwindSafe + FnOnce() -> A>(f: F) -> std::io::Result<A> {
    with_current_directory("tests/async-tests", f)
}

lazy_static! {
    static ref TEST_CRATE_ANALYZED: bool = *helpers::DFPP_INSTALLED
        && do_in_crate_dir(|| { run_dfpp_with_graph_dump() }).map_or_else(
            |e| {
                println!("io err {}", e);
                false
            },
            |t| t
        );
}

macro_rules! ana_test {
    ($name:ident $graph:ident $block:block) => {
        #[test]
        fn $name() {
            assert!(*TEST_CRATE_ANALYZED);
            use_rustc(|| {
                let $graph =
                    do_in_crate_dir(|| G::from_file(Symbol::intern(stringify!($name)))).unwrap();
                $block
            });
        }
    };
}

ana_test!(top_level_inlining_happens graph {
    let get = graph.function_call("get_user_data");
    let dp = graph.function_call("dp_user_data");
    let send = graph.function_call("send_user_data");

    assert!(graph.connects(&get, &dp));
    assert!(graph.connects(&dp, &send));
    assert!(graph.connects(&get, &send));
    assert!(!graph.connects_direct(&get, &send))
});

ana_test!(awaiting_works graph {
    let get = graph.function_call("async_get_user_data");
    let dp = graph.function_call("async_dp_user_data");
    let send = graph.function_call("async_send_user_data");

    assert!(graph.connects(&get, &dp));
    assert!(graph.connects(&dp, &send));
    assert!(graph.connects(&get, &send));
    assert!(!graph.connects_direct(&get, &send))
});

ana_test!(two_data_over_boundary graph {
    let get = graph.function_call(" get_user_data(");
    let get2 = graph.function_call("get_user_data2");
    let send = graph.function_call("send_user_data(");
    let send2 = graph.function_call("send_user_data2");

    assert!(graph.connects(&get, &send));
    assert!(graph.connects(&get2, &send2));
    assert!(!graph.connects(&get, &send2));
    assert!(!graph.connects(&get2, &send));
});

ana_test!(inlining_crate_local_async_fns graph {

    let get = graph.function_call("get_user_data");
    let dp = graph.function_call(" dp_user_data");
    let send = graph.function_call("send_user_data");

    assert!(graph.connects(&get, &dp));
    assert!(graph.connects(&dp, &send));
    assert!(graph.connects(&get, &send));
    assert!(!graph.connects_direct(&get, &send))
});
ana_test!(arguments_work graph {
    let send = graph.function_call("send_user_data");
    let data = graph.argument(graph.ctrl(), 0);
    assert!(graph.connects(&(data, send.1), &send));
});
