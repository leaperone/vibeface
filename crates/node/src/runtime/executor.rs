//! How each provider runs turns on this device: dispatched work, and messages steered into a
//! running reply. The Node journals a work before anything is sent and, when a result is
//! unknown, reads it back instead of sending again.
use super::*;
use crate::scheduling::{SteerOutcome, SteerTarget};
use crate::work::{Probe, WorkRecord, WorkSpec};
use futures_util::future::BoxFuture;

/// How the provider took a steer.
#[derive(Debug, PartialEq)]
pub(crate) enum SteerAck {
    Accepted,
    /// Nothing was sent, as when the turn had just ended.
    Refused(String),
    /// It may have been sent; only the thread tells.
    Unknown,
}

/// Where a steered message went. A provider takes steered input only into a running turn.
#[derive(Debug, PartialEq)]
pub(crate) enum SteerTrace {
    /// The thread does not show the target turn.
    TargetUnseen,
    /// It may still arrive: the turn holding it, or the target turn, still runs.
    Pending,
    /// The target turn ended without it: it never arrived.
    NotArrived,
    /// It is in a turn that ended: answered when that is the completed target turn.
    Ended(SteerOutcome),
}

/// One provider's turns on this device.
pub(crate) trait Executor: Send + Sync {
    /// Starts `work`, registered just now, in `thread` when there is one: the thread it names
    /// or that of the work it continues. `args` are `dispatch_work`'s, with model, effort and
    /// images. The thread is journaled (`Signal::ThreadCreated`) before anything is sent, so a
    /// restart reads the turn back by the work ID. `true` leaves the send to the dispatching
    /// Agent, which reports it through `bind_work`. On an error the reconcile refuses the work
    /// once its send window is over.
    fn start<'a>(
        &'a self,
        node: &'a Arc<Runtime>,
        network: &'a str,
        work: &'a WorkSpec,
        thread: Option<&'a str>,
        args: &'a Value,
    ) -> BoxFuture<'a, Result<bool>>;
    /// Reads the turn of a work that has a thread: the bound one, or the one carrying the work
    /// ID. It never starts or sends anything.
    fn probe<'a>(&'a self, work: &'a WorkRecord) -> BoxFuture<'a, Result<Probe>>;
    /// Asks the work's running turn to stop; `Ok` once the provider took it. The turn's end
    /// confirms the stop.
    fn interrupt<'a>(&'a self, work: &'a WorkRecord) -> BoxFuture<'a, Result<()>>;
    /// Sends a message into the running turn `into`, keyed by `message_id`.
    fn steer<'a>(
        &'a self,
        into: &'a SteerTarget,
        message_id: &'a str,
        text: &'a str,
    ) -> BoxFuture<'a, SteerAck>;
    /// Where the message steered into `into` went; `None` when the thread could not be read.
    fn read_back<'a>(
        &'a self,
        into: &'a SteerTarget,
        message_id: &'a str,
    ) -> BoxFuture<'a, Option<SteerTrace>>;
}

impl Runtime {
    /// Where `provider`'s turns run on this device.
    pub(super) fn executor(&self, provider: Provider) -> Result<&dyn Executor> {
        match provider {
            Provider::Codex => Ok(self.codex.as_ref()),
            Provider::Cursor | Provider::Claude | Provider::Pi | Provider::Opencode => {
                bail!("{} runs no work on this device yet", provider.label())
            }
        }
    }

    pub(super) fn work_executor(&self, work: &WorkRecord) -> Result<&dyn Executor> {
        match work.kind {
            WorkKind::Agent(provider) => self.executor(provider),
            WorkKind::Task => bail!("an agent task runs as its bot's turn"),
        }
    }
}
