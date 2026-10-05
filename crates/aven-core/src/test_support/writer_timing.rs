//! Task-scoped measurements of SQLite writer-gate and transaction durations.
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use sqlx::{Sqlite, SqliteConnection, Transaction};

tokio::task_local! {
    static SAMPLES: Arc<Mutex<WriterSamples>>;
}

#[derive(Clone, Debug, Default)]
pub struct WriterSamples {
    /// Time holding the in-process writer gate and its pool connection.
    pub gate_holds: Vec<Duration>,
    /// Time acquiring the in-process writer gate and a pool connection.
    pub gate_waits: Vec<Duration>,
    /// Successful-BEGIN through completion of the awaited COMMIT call.
    pub transaction_commits: Vec<Duration>,
    /// Successful-BEGIN through completion of an explicit awaited ROLLBACK.
    pub transaction_rollbacks: Vec<Duration>,
    /// Successful-BEGIN through transaction drop / rollback dispatch. This
    /// does not measure completion of SQLx's asynchronous rollback.
    pub transaction_drop_dispatches: Vec<Duration>,
}

/// Measures writer activity in this task and returns its samples.
pub async fn measure<T>(future: impl std::future::Future<Output = T>) -> (T, WriterSamples) {
    let samples = Arc::new(Mutex::new(WriterSamples::default()));
    let result = SAMPLES.scope(samples.clone(), future).await;
    let samples = samples.lock().unwrap().clone();
    (result, samples)
}

pub(crate) fn record_gate_hold(duration: Duration) {
    if let Ok(samples) = SAMPLES.try_with(Arc::clone) {
        samples.lock().unwrap().gate_holds.push(duration);
    }
}

pub(crate) fn record_gate_wait(duration: Duration) {
    if let Ok(samples) = SAMPLES.try_with(Arc::clone) {
        samples.lock().unwrap().gate_waits.push(duration);
    }
}

pub(crate) struct TimedTransaction<'a> {
    transaction: Option<Transaction<'a, Sqlite>>,
    started: Instant,
    samples: Option<Arc<Mutex<WriterSamples>>>,
    recorded: bool,
}

impl<'a> TimedTransaction<'a> {
    /// The timer starts after SQLite has successfully begun the transaction.
    pub(crate) fn new(transaction: Transaction<'a, Sqlite>) -> Self {
        Self {
            transaction: Some(transaction),
            started: Instant::now(),
            samples: SAMPLES.try_with(Arc::clone).ok(),
            recorded: false,
        }
    }

    pub(crate) async fn commit(mut self) -> sqlx::Result<()> {
        let result = self.transaction.take().unwrap().commit().await;
        if let Some(samples) = &self.samples {
            samples
                .lock()
                .unwrap()
                .transaction_commits
                .push(self.started.elapsed());
        }
        self.recorded = true;
        result
    }

    pub(crate) async fn rollback(mut self) -> sqlx::Result<()> {
        let result = self.transaction.take().unwrap().rollback().await;
        if let Some(samples) = &self.samples {
            samples
                .lock()
                .unwrap()
                .transaction_rollbacks
                .push(self.started.elapsed());
        }
        self.recorded = true;
        result
    }
}

impl std::ops::Deref for TimedTransaction<'_> {
    type Target = SqliteConnection;
    fn deref(&self) -> &Self::Target {
        self.transaction.as_ref().unwrap()
    }
}

impl std::ops::DerefMut for TimedTransaction<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.transaction.as_mut().unwrap()
    }
}

impl Drop for TimedTransaction<'_> {
    fn drop(&mut self) {
        if self.recorded {
            return;
        }
        drop(self.transaction.take());
        if let Some(samples) = &self.samples {
            samples
                .lock()
                .unwrap()
                .transaction_drop_dispatches
                .push(self.started.elapsed());
        }
    }
}
