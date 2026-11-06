//! tracedb's run model: events, merged runs, the merge rule and validation. No I/O.

pub mod derive;
pub mod dotted_order;
pub mod event;
pub mod merge;
pub mod run;
pub mod ts;
pub mod validate;
pub mod views;

pub use derive::derive;
pub use dotted_order::DottedOrder;
pub use event::{Op, RunEvent, Source};
pub use merge::{ChangeKind, merge};
pub use run::{
    DEFAULT_PROJECT, JsonMap, PayloadSizes, Run, RunFields, RunId, RunState, RunType, Status, TraceId, Warning,
};
pub use ts::Ts;
pub use validate::{Reason, Reject, validate};
pub use views::{RootsPage, RunHeader, RunView, TraceView};
