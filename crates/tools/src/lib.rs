//! Tools around a tracedb server: trace generation (`seed`), the overnight-eval scenario, the query CLI, the
//! scenario checks (`demo`) and the benchmark (`bench`). The server never depends on this crate.

pub mod bench;
pub mod cli_query;
pub mod demo;
pub mod scenario;
pub mod seed;
