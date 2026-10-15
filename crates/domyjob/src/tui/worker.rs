use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};

use super::model::{Model, Request};
use crate::chat::store::observer::{self, Snapshot};

#[derive(Debug)]
struct Work {
    generation: u64,
    request: Request,
}

#[derive(Debug)]
struct Completed {
    work: Box<Work>,
    result: Result<Snapshot, String>,
}

#[derive(Debug)]
pub(super) struct Worker {
    requests: Option<SyncSender<Box<Work>>>,
    completed: Receiver<Completed>,
    cancelled: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Worker {
    pub(super) fn start() -> io::Result<Self> {
        let state = crate::layout::State::here()?;
        Self::with_observer(move |request| {
            observer::observe(&state, request).map_err(|error| error.to_string())
        })
    }

    pub(super) fn with_observer(
        observe: impl Fn(&Request) -> Result<Snapshot, String> + Send + 'static,
    ) -> io::Result<Self> {
        let (requests, pending) = sync_channel::<Box<Work>>(1);
        let (results, completed) = sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let worker_cancelled = Arc::clone(&cancelled);
        let thread = std::thread::Builder::new()
            .name("domyjob-observer".to_owned())
            .spawn(move || {
                while let Ok(work) = pending.recv() {
                    if worker_cancelled.load(Ordering::Acquire) {
                        break;
                    }
                    let result = observe(&work.request);
                    if worker_cancelled.load(Ordering::Acquire) {
                        break;
                    }
                    if results.send(Completed { work, result }).is_err() {
                        break;
                    }
                }
            })?;
        Ok(Self {
            requests: Some(requests),
            completed,
            cancelled,
            thread: Some(thread),
        })
    }

    pub(super) fn refresh(&self, model: &mut Model) {
        let Some(generation) = model.refresh.request() else {
            return;
        };
        let work = Box::new(Work {
            generation,
            request: model.request.clone(),
        });
        let result = self.requests.as_ref().map(|sender| sender.try_send(work));
        match result {
            Some(Ok(())) => {}
            Some(Err(TrySendError::Full(_))) => {
                model.accept(
                    generation,
                    &model.request.clone(),
                    Err("observer queue busy".to_owned()),
                );
            }
            Some(Err(TrySendError::Disconnected(_))) | None => {
                model.accept(
                    generation,
                    &model.request.clone(),
                    Err("observer stopped".to_owned()),
                );
            }
        }
    }

    pub(super) fn drain(&self, model: &mut Model) {
        match self.completed.try_recv() {
            Ok(completed) => model.accept(
                completed.work.generation,
                &completed.work.request,
                completed.result,
            ),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => model.error = Some("observer stopped".to_owned()),
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
        drop(self.requests.take());
        if let Some(thread) = self.thread.take()
            && thread.is_finished()
            && thread.join().is_err()
        {
            eprintln!("domyjob: observer thread failed during shutdown");
        }
    }
}

#[cfg(test)]
mod tests;
