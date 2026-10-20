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

use std::any::Any;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use crate::cli::EcHw;

/// One queued driver call. `run` receives the client to call into and
/// produces a type-erased reply; `FencedEc`-style generation checks are
/// unnecessary because dispatch order is total.
/// A queued driver call. Boxed so the unbounded-in-type closures all fit
/// one channel; the reply carries the typed result back via `Any`.
type BoxedEcCall = Box<dyn FnOnce(Arc<dyn EcHw>) -> Box<dyn Any + Send> + Send>;

struct Job {
    op: &'static str,
    client: Arc<dyn EcHw>,
    run: BoxedEcCall,
    reply: oneshot::Sender<JobReply>,
}

enum JobReply {
    Value(Box<dyn Any + Send>),
    Failed(String),
}

/// Max queued ops; beyond this `exec` fails fast instead of piling up
/// behind a hung driver. 16 is far above the healthy steady state
/// (background loop + the occasional UI write).
const QUEUE_CAP: usize = 16;

fn worker_handle() -> mpsc::Sender<Job> {
    static TX: std::sync::OnceLock<mpsc::Sender<Job>> = std::sync::OnceLock::new();
    TX.get_or_init(|| {
        let (tx, rx) = mpsc::channel(QUEUE_CAP);
        tokio::spawn(worker_loop(rx));
        tx
    })
    .clone()
}

async fn worker_loop(mut rx: mpsc::Receiver<Job>) {
    while let Some(job) = rx.recv().await {
        tracing::trace!("EC dispatch {}", job.op);
        // The blocking call runs on the pool, but this task does not take
        // the next job until it finishes: at most one driver call is ever
        // in flight, so hardware execution order always matches dispatch
        // order and a late orphan cannot overtake a newer write.
        let handle = tokio::task::spawn_blocking(move || (job.run)(job.client));
        match handle.await {
            Ok(any) => {
                let _ = job.reply.send(JobReply::Value(any));
            }
            Err(e) => {
                let _ = job
                    .reply
                    .send(JobReply::Failed(format!("EC task panicked: {e}")));
            }
        }
    }
}

/// Runs `f` against the EC on the single writer thread and awaits its
/// reply, up to `timeout`. The closure receives the client to call into,
/// so call sites keep their existing `move |ec| ec.X(...)` shape.
///
/// Contention cannot happen by construction (one writer, FIFO), so unlike
/// the old lock-based path there is no "contended" case: `Err` means the
/// driver call failed, panicked, timed out, or the queue was full.
pub async fn exec<F, T>(
    op: &'static str,
    timeout: Duration,
    client: Arc<dyn EcHw>,
    f: F,
) -> Result<T, String>
where
    F: FnOnce(Arc<dyn EcHw>) -> T + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    let job = Job {
        op,
        client,
        run: Box::new(move |ec| {
            let out: T = f(ec);
            Box::new(out) as Box<dyn Any + Send>
        }),
        reply: tx,
    };
    crate::ec_serial::worker_handle()
        .try_send(job)
        .map_err(|_| format!("EC overloaded, skipping {op}"))?;
    match tokio::time::timeout(timeout, rx).await {
        Ok(Ok(JobReply::Value(any))) => any.downcast().map(|b| *b).map_err(|_| {
            // Unreachable by construction: the box always holds T.
            format!("EC reply type mismatch for {op}")
        }),
        Ok(Ok(JobReply::Failed(e))) => Err(e),
        Ok(Err(_)) => Err("EC dispatcher gone".to_string()),
        Err(_) => Err(format!("EC {op} timed out after {timeout:?}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ec_wrapper::tests::MockEc;

    /// Independent writer per test (not the prod global): a hang in one
    /// test must not stall another.
    fn local_worker() -> (mpsc::Sender<Job>, tokio::task::JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(QUEUE_CAP);
        let h = tokio::spawn(worker_loop(rx));
        (tx, h)
    }

    async fn local_exec<F, T>(
        tx: &mpsc::Sender<Job>,
        op: &'static str,
        timeout: Duration,
        client: Arc<dyn EcHw>,
        f: F,
    ) -> Result<T, String>
    where
        F: FnOnce(Arc<dyn EcHw>) -> T + Send + 'static,
        T: Send + 'static,
    {
        let (rtx, rrx) = oneshot::channel();
        tx.try_send(Job {
            op,
            client,
            run: Box::new(move |ec| {
                let out: T = f(ec);
                Box::new(out) as Box<dyn Any + Send>
            }),
            reply: rtx,
        })
        .map_err(|_| format!("EC overloaded, skipping {op}"))?;
        match tokio::time::timeout(timeout, rrx).await {
            Ok(Ok(JobReply::Value(any))) => any
                .downcast()
                .map(|b| *b)
                .map_err(|_| format!("EC reply type mismatch for {op}")),
            Ok(Ok(JobReply::Failed(e))) => Err(e),
            Ok(Err(_)) => Err("EC dispatcher gone".to_string()),
            Err(_) => Err(format!("EC {op} timed out after {timeout:?}")),
        }
    }

    #[tokio::test]
    async fn ops_execute_in_submit_order() {
        let mock = Arc::new(MockEc::new());
        let client: Arc<dyn EcHw> = mock.clone();
        let (tx, _h) = local_worker();
        for i in 0..5u32 {
            let c = client.clone();
            local_exec(&tx, "op", Duration::from_secs(5), c, move |ec| {
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
        let first = local_exec(&tx, "hang", Duration::from_millis(50), c, move |ec| {
            ec.set_fan_duty(80, None)
        })
        .await;
        assert!(first.is_err(), "hung op must time out, got {first:?}");
        mock.set_hang(false);
        let c = client.clone();
        let second = local_exec(&tx, "after", Duration::from_secs(5), c, move |ec| {
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
            let _ = local_exec(&tx_hung, "hang", Duration::from_secs(30), c, move |ec| {
                ec.set_fan_duty(80, None)
            })
            .await;
        });
        // Let the worker pick it up.
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Fill the queue past capacity.
        let mut rejected = 0;
        for _ in 0..(QUEUE_CAP + 4) {
            let c = client.clone();
            let r = local_exec(&tx, "filler", Duration::from_millis(10), c, move |ec| {
                ec.set_fan_duty(0, None)
            })
            .await;
            if r.is_err() {
                rejected += 1;
            }
        }
        assert!(rejected > 0, "queue must shed load when wedged");
        mock.set_hang(false);
    }
}
