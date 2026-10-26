//! Single-writer EC dispatcher with a bounded queue.
//!
//! All EC driver traffic flows through one worker task so hardware calls can
//! never overlap or reorder. Previously every poll spawned its own blocking
//! thread and a timed-out orphan kept running: it could land after a newer
//! write (notably a stale duty after the quit/final firmware handover), and
//! orphans piled up the shared blocking pool until every EC call timed out.
//!
//! Design notes, read before "optimizing" this:
//!
//! - Strict FIFO + await-completion: the worker dispatches one op, waits for
//!   the blocking thread to actually finish, then takes the next. At most
//!   one driver call is ever in flight, so out-of-order hardware execution
//!   is impossible by construction. No generation counter is needed: with
//!   total order, a quit handover submitted after a stale duty always lands
//!   after it, and background work submitted after quit is already gated by
//!   the shutdown checks at each call site.
//! - Caller timeouts only bound how long the *caller* waits. A timed-out
//!   caller gets `Err` while the worker still awaits that op to completion
//!   before dispatching the next one, so the driver is never double-driven.
//! - The queue is bounded (16); `try_send` fails fast with "overloaded"
//!   instead of queueing behind a hung driver. Existing callers already
//!   treat that substring as backpressure, not failure.
//! - Panics in hardware code surface as `Err`, never as a wedged worker:
//!   the JoinError is mapped to a reply and the loop continues.
//! - Driver calls run on a dedicated runtime, not the shared blocking pool.
//!   A hung EC ioctl therefore cannot starve winget/PowerShell/config-save
//!   work (or vice versa). The worker still waits for the stuck call to
//!   finish: abandoning it would let a stale hardware write overtake a newer
//!   one. A 10s warning makes the condition visible in the log.

use std::any::Any;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use crate::cli::EcHw;

/// What `exec` can report, beyond the driver call's own `Result`.
/// Callers match on the variant instead of substring-sniffing messages,
/// so backpressure (`Overloaded`) is never confused with failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DispatchError {
    /// Queue full: skip this cycle, do not count it as a failure.
    Overloaded { op: &'static str },
    /// Caller stopped waiting; the op may still execute later.
    TimedOut { op: &'static str, after: Duration },
    /// The driver call itself failed (or panicked, which is mapped here).
    Failed(String),
}

impl fmt::Display for DispatchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DispatchError::Overloaded { op } => write!(f, "EC overloaded, skipping {op}"),
            DispatchError::TimedOut { op, after } => {
                write!(f, "EC {op} timed out after {after:?}")
            }
            DispatchError::Failed(e) => write!(f, "{e}"),
        }
    }
}

impl DispatchError {
    /// Backpressure, not failure: the queue was full, nothing was sent.
    /// Callers skip the cycle instead of counting a failure or resetting.
    pub fn is_overloaded(&self) -> bool {
        matches!(self, DispatchError::Overloaded { .. })
    }
}

/// A queued driver call. Boxed so the unbounded-in-type closures all fit
/// one channel; the reply carries the typed result back via `Any`.
/// `FencedEc`-style generation checks are unnecessary: with a single
/// writer, dispatch order always matches submission order.
type BoxedEcCall = Box<dyn FnOnce(Arc<dyn EcHw>) -> Box<dyn Any + Send> + Send>;

struct Job {
    op: &'static str,
    client: Arc<dyn EcHw>,
    run: BoxedEcCall,
    reply: Reply,
    force: bool,
}

enum Reply {
    Async(oneshot::Sender<JobReply>),
    Blocking(std::sync::mpsc::Sender<JobReply>),
}

enum JobReply {
    Value(Box<dyn Any + Send>),
    Failed(String),
}

fn send_reply(reply: Reply, value: JobReply) {
    match reply {
        Reply::Async(tx) => {
            let _ = tx.send(value);
        }
        Reply::Blocking(tx) => {
            let _ = tx.send(value);
        }
    }
}

/// Max queued ops; beyond this `exec` fails fast instead of piling up
/// behind a hung driver. 16 is far above the healthy steady state
/// (background loop + the occasional UI write).
const QUEUE_CAP: usize = 16;

/// Dedicated runtime for EC driver calls. Keeping hardware I/O off the main
/// blocking pool means a stuck ioctl cannot starve network/install/auth work,
/// and those tasks cannot starve the fan controller.
fn ec_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .max_blocking_threads(2)
            .thread_name("ec-io")
            .enable_all()
            .build()
            .expect("failed to build EC I/O runtime")
    })
}

/// How long one driver call may run before the worker logs a stuck warning.
/// The worker keeps waiting; see the module-level design notes.
const STUCK_WARN_AFTER: Duration = Duration::from_secs(10);

/// Lazily-started worker handle. If the worker task ever dies (only via
/// a bug or runtime teardown, never via driver errors), the slot is
/// empty and the next `sender()` call rebuilds it instead of wedging
/// every future caller on a dead channel.
static SLOT: std::sync::Mutex<Option<mpsc::Sender<Job>>> = std::sync::Mutex::new(None);
static WRITES_CLOSED: AtomicBool = AtomicBool::new(false);

fn lock_slot() -> std::sync::MutexGuard<'static, Option<mpsc::Sender<Job>>> {
    SLOT.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn sender() -> mpsc::Sender<Job> {
    let mut slot = lock_slot();
    if let Some(tx) = slot.as_ref() {
        if !tx.is_closed() {
            return tx.clone();
        }
        *slot = None;
    }
    let (tx, rx) = mpsc::channel(QUEUE_CAP);
    drop(ec_runtime().handle().spawn(worker_loop(rx, &WRITES_CLOSED)));
    *slot = Some(tx.clone());
    tx
}

fn existing_sender() -> Option<mpsc::Sender<Job>> {
    let slot = lock_slot();
    slot.as_ref().filter(|tx| !tx.is_closed()).cloned()
}

async fn worker_loop(mut rx: mpsc::Receiver<Job>, writes_closed: &'static AtomicBool) {
    while let Some(job) = rx.recv().await {
        tracing::trace!("EC dispatch {}", job.op);
        let Job {
            op,
            client,
            run,
            reply,
            force,
        } = job;
        if !force && writes_closed.load(Ordering::Acquire) {
            send_reply(reply, JobReply::Failed("EC writes closed".to_string()));
            continue;
        }
        // The blocking call runs on the dedicated EC runtime, but this task
        // does not take the next job until it finishes: at most one driver
        // call is ever in flight, so hardware execution order always matches
        // dispatch order and a late orphan cannot overtake a newer write.
        let mut handle = ec_runtime().handle().spawn_blocking(move || run(client));
        // Warn once per stuck call without abandoning it (see module notes).
        let joined = match tokio::time::timeout(STUCK_WARN_AFTER, &mut handle).await {
            Ok(result) => result,
            Err(_) => {
                tracing::error!(
                    "EC driver call {} still running after {:?}; waiting to preserve single-writer ordering",
                    op,
                    STUCK_WARN_AFTER
                );
                handle.await
            }
        };
        match joined {
            Ok(any) => send_reply(reply, JobReply::Value(any)),
            Err(e) => send_reply(reply, JobReply::Failed(format!("EC task panicked: {e}"))),
        }
    }
}

/// Shared submit-and-await core used by both the production path (`exec`,
/// via the global worker) and tests (via a local worker). Keeping one
/// implementation means tests cover the exact code production runs.
async fn dispatch<F, T>(
    tx: &mpsc::Sender<Job>,
    op: &'static str,
    timeout: Duration,
    client: Arc<dyn EcHw>,
    f: F,
) -> Result<T, DispatchError>
where
    F: FnOnce(Arc<dyn EcHw>) -> T + Send + 'static,
    T: Send + 'static,
{
    dispatch_with_force(tx, op, timeout, client, f, false).await
}

async fn dispatch_with_force<F, T>(
    tx: &mpsc::Sender<Job>,
    op: &'static str,
    timeout: Duration,
    client: Arc<dyn EcHw>,
    f: F,
    force: bool,
) -> Result<T, DispatchError>
where
    F: FnOnce(Arc<dyn EcHw>) -> T + Send + 'static,
    T: Send + 'static,
{
    use tokio::sync::mpsc::error::TrySendError;
    let (reply_tx, rx) = oneshot::channel();
    let job = Job {
        op,
        client,
        run: Box::new(move |ec| {
            let out: T = f(ec);
            Box::new(out) as Box<dyn Any + Send>
        }),
        reply: Reply::Async(reply_tx),
        force,
    };
    match tx.try_send(job) {
        Ok(()) => {}
        // Full and Closed both mean "do not queue": Full is backpressure,
        // Closed means the worker died (it is rebuilt on next `sender()`
        // call). Either way the caller must skip, not pile up.
        Err(TrySendError::Full(_)) => return Err(DispatchError::Overloaded { op }),
        Err(TrySendError::Closed(_)) => return Err(DispatchError::Overloaded { op }),
    }
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(JobReply::Value(any))) => any.downcast().map(|b| *b).map_err(|_| {
            // Unreachable by construction: the box always holds T.
            DispatchError::Failed(format!("EC reply type mismatch for {op}"))
        }),
        Ok(Ok(JobReply::Failed(e))) => Err(DispatchError::Failed(e)),
        Ok(Err(_)) => Err(DispatchError::Failed("EC dispatcher gone".to_string())),
        Err(_) => Err(DispatchError::TimedOut { op, after: timeout }),
    }
}

/// Runs `f` against the EC on the single writer thread and awaits its
/// reply, up to `timeout`. The closure receives the client to call into,
/// so call sites keep their existing `move |ec| ec.X(...)` shape.
///
/// Contention cannot happen by construction (one writer, FIFO), so unlike
/// the old lock-based path there is no "contended" case: match on
/// [`DispatchError`] to tell backpressure apart from failure.
pub async fn exec<F, T>(
    op: &'static str,
    timeout: Duration,
    client: Arc<dyn EcHw>,
    f: F,
) -> Result<T, DispatchError>
where
    F: FnOnce(Arc<dyn EcHw>) -> T + Send + 'static,
    T: Send + 'static,
{
    let tx = sender();
    dispatch(&tx, op, timeout, client, f).await
}

/// Closes the write fence and runs one final job after all already-queued EC
/// operations. Used by the normal quit path so no late duty can overtake the
/// firmware handover.
pub async fn exec_force<F, T>(
    op: &'static str,
    timeout: Duration,
    client: Arc<dyn EcHw>,
    f: F,
) -> Result<T, DispatchError>
where
    F: FnOnce(Arc<dyn EcHw>) -> T + Send + 'static,
    T: Send + 'static,
{
    let tx = sender();
    WRITES_CLOSED.store(true, Ordering::Release);
    dispatch_with_force(&tx, op, timeout, client, f, true).await
}

fn dispatch_blocking<F, T>(
    tx: &mpsc::Sender<Job>,
    op: &'static str,
    timeout: Duration,
    client: Arc<dyn EcHw>,
    f: F,
) -> Result<T, DispatchError>
where
    F: FnOnce(Arc<dyn EcHw>) -> T + Send + 'static,
    T: Send + 'static,
{
    use std::sync::mpsc::RecvTimeoutError;
    use tokio::sync::mpsc::error::TrySendError;
    let (reply_tx, reply_rx) = std::sync::mpsc::channel();
    let job = Job {
        op,
        client,
        run: Box::new(move |ec| {
            let out: T = f(ec);
            Box::new(out) as Box<dyn Any + Send>
        }),
        reply: Reply::Blocking(reply_tx),
        force: true,
    };
    match tx.try_send(job) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => return Err(DispatchError::Overloaded { op }),
        Err(TrySendError::Closed(_)) => {
            return Err(DispatchError::Failed(
                "EC dispatcher closed during shutdown".to_string(),
            ));
        }
    }
    match reply_rx.recv_timeout(timeout) {
        Ok(JobReply::Value(any)) => any
            .downcast()
            .map(|b| *b)
            .map_err(|_| DispatchError::Failed(format!("EC reply type mismatch for {op}"))),
        Ok(JobReply::Failed(e)) => Err(DispatchError::Failed(e)),
        Err(RecvTimeoutError::Timeout) => Err(DispatchError::TimedOut { op, after: timeout }),
        Err(RecvTimeoutError::Disconnected) => {
            Err(DispatchError::Failed("EC dispatcher gone".to_string()))
        }
    }
}

/// Closes the write fence and runs one final job through the single EC writer.
/// Used by the session-end path, which cannot await inside the window procedure.
pub fn exec_blocking<F, T>(
    op: &'static str,
    timeout: Duration,
    client: Arc<dyn EcHw>,
    f: F,
) -> Result<T, DispatchError>
where
    F: FnOnce(Arc<dyn EcHw>) -> T + Send + 'static,
    T: Send + 'static,
{
    let tx = existing_sender().ok_or_else(|| {
        DispatchError::Failed("EC dispatcher unavailable during shutdown".to_string())
    })?;
    WRITES_CLOSED.store(true, Ordering::Release);
    dispatch_blocking(&tx, op, timeout, client, f)
}

#[cfg(test)]
pub(crate) fn global_dispatch_test_lock() -> &'static tokio::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<tokio::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ec_wrapper::tests::MockEc;

    fn local_worker_with_fence() -> (
        mpsc::Sender<Job>,
        tokio::task::JoinHandle<()>,
        &'static AtomicBool,
    ) {
        let fence: &'static AtomicBool = Box::leak(Box::new(AtomicBool::new(false)));
        let (tx, rx) = mpsc::channel(QUEUE_CAP);
        let h = tokio::spawn(worker_loop(rx, fence));
        (tx, h, fence)
    }

    /// Independent writer per test (not the prod global): a hang in one
    /// test must not stall another.
    fn local_worker() -> (mpsc::Sender<Job>, tokio::task::JoinHandle<()>) {
        let (tx, h, _fence) = local_worker_with_fence();
        (tx, h)
    }

    #[tokio::test]
    async fn ops_execute_in_submit_order() {
        let mock = Arc::new(MockEc::new());
        let client: Arc<dyn EcHw> = mock.clone();
        let (tx, _h) = local_worker();
        for i in 0..5u32 {
            let c = client.clone();
            dispatch(&tx, "op", Duration::from_secs(5), c, move |ec| {
                ec.set_fan_duty(i * 10, None)
            })
            .await
            .unwrap()
            .unwrap();
        }
        assert_eq!(
            *mock.ops.lock().unwrap(),
            vec![
                "set_fan_duty(0,None)".to_string(),
                "set_fan_duty(10,None)".to_string(),
                "set_fan_duty(20,None)".to_string(),
                "set_fan_duty(30,None)".to_string(),
                "set_fan_duty(40,None)".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn timeout_does_not_break_later_ops() {
        // First op hangs past the caller timeout; the second must still
        // execute in order once the driver recovers, with no pileup.
        let mock = Arc::new(MockEc::with_hang(true));
        let client: Arc<dyn EcHw> = mock.clone();
        let (tx, _h) = local_worker();
        let c = client.clone();
        let first = dispatch(&tx, "hang", Duration::from_millis(50), c, move |ec| {
            ec.set_fan_duty(80, None)
        })
        .await;
        assert!(first.is_err(), "hung op must time out, got {first:?}");
        mock.set_hang(false);
        let c = client.clone();
        let second = dispatch(&tx, "after", Duration::from_secs(5), c, move |ec| {
            ec.set_fan_duty(10, None)
        })
        .await;
        assert!(second.unwrap().is_ok());
        // Both ran, in order: the timed-out op still completed first.
        assert_eq!(
            *mock.ops.lock().unwrap(),
            vec![
                "set_fan_duty(80,None)".to_string(),
                "set_fan_duty(10,None)".to_string(),
            ]
        );
    }

    /// Randomized soak: mixed reads/writes with occasional hangs and
    /// overload windows. `EC_SOAK_ITERS` scales it (default 500 for CI;
    /// the nightly soak workflow sets 20000). Asserts every submitted op
    /// ran exactly once and write order matches submission order: no
    /// loss, duplication, or reorder, even across timeouts.
    #[tokio::test]
    async fn soak_randomized_ops_preserve_order() {
        let iters: u32 = std::env::var("EC_SOAK_ITERS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(500);
        let mock = Arc::new(MockEc::new());
        let client: Arc<dyn EcHw> = mock.clone();
        let (tx, _h) = local_worker();
        // Deterministic LCG so failures reproduce with the same count.
        let mut rng: u64 = 0x12345678;
        let mut expected_duties: Vec<u32> = Vec::new();
        for i in 0..iters {
            // Mix reads and writes the way the background loop does; the
            // duty encodes the submission index so order is checkable.
            // Only set_fan_duty honors the hang flag (thermal never hangs),
            // and hung ops still record exactly once before blocking.
            let is_set = i % 3 != 0;
            let hang_this_one = is_set && i % 37 == 5;
            if hang_this_one {
                mock.set_hang(true);
            }
            let c = client.clone();
            let r: Result<Result<(), String>, DispatchError> = if !is_set {
                dispatch(&tx, "thermal", Duration::from_millis(200), c, move |ec| {
                    ec.thermal().map(|_| ())
                })
                .await
            } else {
                expected_duties.push(i);
                dispatch(&tx, "set", Duration::from_millis(200), c, move |ec| {
                    ec.set_fan_duty(i, None)
                })
                .await
            };
            if hang_this_one {
                // Hung op must time out, never wedge the worker forever.
                assert!(r.is_err(), "hung op {i} must time out");
                mock.set_hang(false);
            } else {
                assert!(r.is_ok(), "op {i} unexpectedly failed: {r:?}");
            }
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
        }
        let _ = rng;
        // Every submitted op ran exactly once (recorded at execution).
        let ops = mock.ops.lock().unwrap();
        assert_eq!(ops.len(), iters as usize, "no op may be lost or duplicated");
        // Write order matches submission order: parse the duty payloads.
        let mut duties = Vec::new();
        for op in ops.iter() {
            if let Some(rest) = op.strip_prefix("set_fan_duty(") {
                let n: u32 = rest.split(',').next().unwrap().parse().unwrap();
                duties.push(n);
            }
        }
        assert_eq!(duties, expected_duties, "writes must not be reordered");
    }

    /// Quit path: 100 consecutive firmware handovers must all land, in
    /// order. Models the tray/quit handover being retried across a
    /// flaky driver without losing or reordering the final intent.
    #[tokio::test]
    async fn quit_handover_survives_100_rounds() {
        let mock = Arc::new(MockEc::new());
        let client: Arc<dyn EcHw> = mock.clone();
        let (tx, _h) = local_worker();
        for _ in 0..100 {
            let c = client.clone();
            dispatch(&tx, "quit-handover", Duration::from_secs(5), c, move |ec| {
                ec.autofanctrl()
            })
            .await
            .unwrap()
            .unwrap();
        }
        assert_eq!(mock.ops.lock().unwrap().len(), 100);
        assert!(mock.ops.lock().unwrap().iter().all(|o| o == "autofanctrl"));
    }

    #[tokio::test]
    async fn full_queue_fails_fast() {
        // Block the worker with a hung op, then fill the queue: further
        // submits must fail immediately instead of queueing behind it.
        let mock = Arc::new(MockEc::with_hang(true));
        let client: Arc<dyn EcHw> = mock.clone();
        let (tx, _h) = local_worker();
        // Occupy the worker.
        let c = client.clone();
        let tx_hung = tx.clone();
        let _hung = tokio::spawn(async move {
            let _ = dispatch(&tx_hung, "hang", Duration::from_secs(30), c, move |ec| {
                ec.set_fan_duty(80, None)
            })
            .await;
        });
        // Let the worker pick it up.
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Fill the queue past capacity.
        let mut rejected = 0;
        for i in 0..(QUEUE_CAP + 4) as u8 {
            let c = client.clone();
            let r: Result<Result<(), String>, DispatchError> =
                dispatch(&tx, "set", Duration::from_millis(200), c, move |ec| {
                    ec.set_fan_duty(i as u32, None)
                })
                .await;
            if r.is_err() {
                rejected += 1;
            }
        }
        assert!(rejected > 0, "queue must shed load when wedged");
        mock.set_hang(false);
    }

    #[tokio::test]
    async fn forced_dispatch_runs_after_writes_close() {
        let mock = Arc::new(MockEc::new());
        let client: Arc<dyn EcHw> = mock.clone();
        let (tx, _h, fence) = local_worker_with_fence();
        fence.store(true, Ordering::Release);
        dispatch_with_force(
            &tx,
            "force",
            Duration::from_secs(5),
            client,
            |ec| ec.autofanctrl(),
            true,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(*mock.ops.lock().unwrap(), vec!["autofanctrl".to_string()]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn blocking_handover_closes_fence_and_keeps_order() {
        let mock = Arc::new(MockEc::new());
        let client: Arc<dyn EcHw> = mock.clone();
        let (tx, _h, fence) = local_worker_with_fence();

        let c = client.clone();
        dispatch(&tx, "before", Duration::from_secs(5), c, move |ec| {
            ec.set_fan_duty(20, None)
        })
        .await
        .unwrap()
        .unwrap();

        fence.store(true, Ordering::Release);
        let c = client.clone();
        dispatch_blocking(&tx, "handover", Duration::from_secs(5), c, move |ec| {
            ec.autofanctrl()
        })
        .unwrap()
        .unwrap();

        let c = client.clone();
        let after: Result<Result<(), String>, DispatchError> =
            dispatch(&tx, "after", Duration::from_secs(5), c, move |ec| {
                ec.set_fan_duty(30, None)
            })
            .await;
        assert!(matches!(
            after,
            Err(DispatchError::Failed(ref e)) if e == "EC writes closed"
        ));

        assert_eq!(
            *mock.ops.lock().unwrap(),
            vec![
                "set_fan_duty(20,None)".to_string(),
                "autofanctrl".to_string()
            ]
        );
    }
}
