mod process;
pub mod protocol;
mod worker;

pub use process::{
    ClosureHealth, StderrSummary, SubmitError, SupervisorCommand, SupervisorCommandKind,
    SupervisorConfig, SupervisorEvent, SupervisorPollError, SupervisorStartError,
    WorkerCommandSender, WorkerSupervisor, WorkerTerminationReason,
};
pub use protocol::*;
pub use worker::{WorkerError, run_worker};
