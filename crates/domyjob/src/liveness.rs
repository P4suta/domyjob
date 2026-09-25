#![expect(
    clippy::disallowed_types,
    clippy::disallowed_methods,
    reason = "liveness is the one place time may decide something: whether a silent peer is still there"
)]

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub const BEAT: Duration = Duration::from_secs(5);
pub const SILENCE: Duration = Duration::from_secs(45);
pub const FIRST_WORD: Duration = Duration::from_secs(120);

pub const STREAM_BEAT: u32 = u32::MAX;
const STREAM_REPLY: &[u8] = br#"{"reply":"stream"}"#;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stage {
    Answering,
    Streaming,
    Answered,
}

struct Beating<'a> {
    out: &'a mut (dyn Write + Send),
    stage: Stage,
}

pub struct Pulsed<'a> {
    shared: &'a (Mutex<Beating<'a>>, Condvar),
    listener: &'a Listener,
}

impl std::fmt::Debug for Pulsed<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pulsed").finish_non_exhaustive()
    }
}

impl Write for Pulsed<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let mut beating = self
            .shared
            .0
            .lock()
            .map_err(|_poisoned| std::io::Error::other("the reply lock was poisoned"))?;
        if beating.stage == Stage::Answering {
            beating.stage = if bytes.starts_with(STREAM_REPLY) {
                Stage::Streaming
            } else {
                Stage::Answered
            };
        }
        let written = beating.out.write_all(bytes);
        drop(beating);
        if written.is_err() {
            self.listener.leave();
        }
        written.map(|()| bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.shared
            .0
            .lock()
            .map_err(|_poisoned| std::io::Error::other("the reply lock was poisoned"))?
            .out
            .flush()
    }
}

#[derive(Debug)]
#[cfg_attr(
    not(test),
    expect(
        missing_copy_implementations,
        reason = "a helper is handed over once, and tests add a variant that cannot be copied"
    )
)]
pub enum Helper {
    Process(u32),
    #[cfg(test)]
    Probe(std::sync::mpsc::Sender<&'static str>, &'static str),
}

impl Helper {
    fn stop(&self) {
        match self {
            Self::Process(pid) => crate::proc::terminate(*pid),
            #[cfg(test)]
            Self::Probe(probe, name) => match probe.send(name) {
                Ok(()) | Err(_) => {}
            },
        }
    }
}

#[derive(Default)]
struct Listener {
    gone: AtomicBool,
    helpers: Mutex<Vec<(u64, Helper)>>,
    next: AtomicU64,
}

impl Listener {
    fn leave(&self) {
        self.gone.store(true, Ordering::SeqCst);
        let helpers = match self.helpers.lock() {
            Ok(mut helpers) => std::mem::take(&mut *helpers),
            Err(_poisoned) => return,
        };
        for (_, helper) in &helpers {
            helper.stop();
        }
    }
}

std::thread_local! {
    static LISTENER: std::cell::RefCell<Option<Arc<Listener>>> = const { std::cell::RefCell::new(None) };
}

pub fn unless_abandoned<R>(helper: Helper, wait: impl FnOnce() -> R) -> R {
    let Some(listener) = LISTENER.with(|current| current.borrow().clone()) else {
        return wait();
    };
    let key = listener.next.fetch_add(1, Ordering::SeqCst);
    let registered = match listener.helpers.lock() {
        Ok(mut helpers) if !listener.gone.load(Ordering::SeqCst) => {
            helpers.push((key, helper));
            true
        }
        Ok(_) | Err(_) => {
            helper.stop();
            false
        }
    };
    let result = wait();
    if registered && let Ok(mut helpers) = listener.helpers.lock() {
        helpers.retain(|(held, _)| *held != key);
    }
    result
}

fn beat(
    shared: &(Mutex<Beating<'_>>, Condvar),
    done: &AtomicBool,
    every: Duration,
    listener: &Listener,
) {
    let (lock, stopped) = shared;
    let Ok(mut beating) = lock.lock() else {
        return;
    };
    loop {
        if done.load(Ordering::SeqCst) {
            return;
        }
        beating = match stopped.wait_timeout(beating, every) {
            Ok((guard, _)) => guard,
            Err(_poisoned) => return,
        };
        if done.load(Ordering::SeqCst) {
            return;
        }
        let pulse: &[u8] = match beating.stage {
            Stage::Answering => b"\n",
            Stage::Streaming => &STREAM_BEAT.to_be_bytes(),
            Stage::Answered => continue,
        };
        if beating
            .out
            .write_all(pulse)
            .and_then(|()| beating.out.flush())
            .is_err()
        {
            drop(beating);
            listener.leave();
            return;
        }
    }
}

pub fn with_pulse<R>(out: &mut (dyn Write + Send), body: impl FnOnce(&mut Pulsed<'_>) -> R) -> R {
    with_pulse_every(out, BEAT, body)
}

fn with_pulse_every<R>(
    out: &mut (dyn Write + Send),
    every: Duration,
    body: impl FnOnce(&mut Pulsed<'_>) -> R,
) -> R {
    let shared = (
        Mutex::new(Beating {
            out,
            stage: Stage::Answering,
        }),
        Condvar::new(),
    );
    let done = AtomicBool::new(false);
    let listener = Arc::new(Listener::default());
    let outer = LISTENER.with(|current| current.replace(Some(Arc::clone(&listener))));
    std::thread::scope(|scope| {
        let heart = scope.spawn(|| beat(&shared, &done, every, &listener));
        let result = body(&mut Pulsed {
            shared: &shared,
            listener: &listener,
        });
        LISTENER.with(|current| current.replace(outer));
        match shared.0.lock() {
            Ok(held) => {
                done.store(true, Ordering::SeqCst);
                drop(held);
            }
            Err(_poisoned) => done.store(true, Ordering::SeqCst),
        }
        shared.1.notify_all();
        match heart.join() {
            Ok(()) | Err(_) => {}
        }
        result
    })
}

#[derive(Debug, Clone, Default)]
pub struct Activity {
    sent: Arc<AtomicU64>,
    received: Arc<AtomicU64>,
}

impl Activity {
    pub fn touch(&self) {
        self.received.fetch_add(1, Ordering::SeqCst);
    }

    pub fn sent(&self) {
        self.sent.fetch_add(1, Ordering::SeqCst);
    }

    fn seen(&self) -> (u64, u64) {
        (
            self.sent.load(Ordering::SeqCst),
            self.received.load(Ordering::SeqCst),
        )
    }
}

#[derive(Debug)]
pub struct Counted<T> {
    inner: T,
    activity: Activity,
}

impl<T> Counted<T> {
    pub const fn new(inner: T, activity: Activity) -> Self {
        Self { inner, activity }
    }
}

impl<T: std::io::Read> std::io::Read for Counted<T> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let read = self.inner.read(buf)?;
        if read > 0 {
            self.activity.touch();
        }
        Ok(read)
    }
}

impl<T: Write> Write for Counted<T> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buf)?;
        if written > 0 {
            self.activity.sent();
        }
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Debug)]
pub struct Watchdog {
    stop: Arc<(Mutex<bool>, Condvar)>,
    silenced: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub first_word: Duration,
    pub silence: Duration,
}

pub const LIMITS: Limits = Limits {
    first_word: FIRST_WORD,
    silence: SILENCE,
};

impl Watchdog {
    pub fn guard(
        activity: &Activity,
        limits: Limits,
        silence: impl FnOnce() + Send + 'static,
    ) -> Self {
        let stop = Arc::new((Mutex::new(false), Condvar::new()));
        let silenced = Arc::new(AtomicBool::new(false));
        let watching = (Arc::clone(&stop), Arc::clone(&silenced), activity.clone());
        let thread = std::thread::spawn(move || {
            let (signal, quieted, heard_from) = watching;
            let (lock, stopped) = &*signal;
            let Ok(mut done) = lock.lock() else {
                return;
            };
            let mut last = heard_from.seen();
            let mut quiet_since = Instant::now();
            let mut heard = false;
            while !*done {
                done = match stopped.wait_timeout(done, limits.silence.min(BEAT)) {
                    Ok((guard, _)) => guard,
                    Err(_poisoned) => return,
                };
                let now = heard_from.seen();
                if now != last {
                    heard |= now.1 != last.1;
                    last = now;
                    quiet_since = Instant::now();
                    continue;
                }
                let allowed = if heard {
                    limits.silence
                } else {
                    limits.first_word
                };
                if !*done && quiet_since.elapsed() >= allowed {
                    quieted.store(true, Ordering::SeqCst);
                    drop(done);
                    silence();
                    return;
                }
            }
        });
        Self {
            stop,
            silenced,
            thread: Some(thread),
        }
    }

    #[must_use]
    pub fn silenced(&self) -> bool {
        self.silenced.load(Ordering::SeqCst)
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        if let Ok(mut done) = self.stop.0.lock() {
            *done = true;
        }
        self.stop.1.notify_all();
        if let Some(thread) = self.thread.take() {
            match thread.join() {
                Ok(()) | Err(_) => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pause(for_how_long: Duration) {
        let still = (Mutex::new(()), Condvar::new());
        let guard = still.0.lock().unwrap();
        drop(still.1.wait_timeout(guard, for_how_long).unwrap());
    }

    const QUICK: Duration = Duration::from_millis(20);

    #[test]
    fn a_reply_written_through_the_pulse_arrives_intact_and_finishes_at_once() {
        let mut out = Vec::new();
        let started = Instant::now();
        let answered = with_pulse(&mut out, |pulsed| {
            pulsed.write_all(b"{\"reply\":\"job\"}\n").unwrap();
            pulsed.flush().unwrap();
            7
        });
        assert_eq!(answered, 7);
        assert_eq!(out, b"{\"reply\":\"job\"}\n");
        assert!(started.elapsed() < BEAT);
    }

    struct Hungup;

    impl Write for Hungup {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::BrokenPipe.into())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_helper_is_stopped_when_the_listener_hangs_up_and_not_otherwise() {
        let (stop, stopped) = std::sync::mpsc::channel();
        let heard = with_pulse_every(&mut Hungup, QUICK, |_| {
            unless_abandoned(Helper::Probe(stop.clone(), "stopped"), || {
                stopped.recv().unwrap()
            })
        });
        assert_eq!(heard, "stopped");

        let after = with_pulse_every(&mut Hungup, BEAT, |pulsed| {
            pulsed.write_all(b"{}\n").unwrap_err();
            unless_abandoned(Helper::Probe(stop.clone(), "stopped"), || {
                stopped.try_recv().unwrap()
            })
        });
        assert_eq!(after, "stopped");

        let mut out = Vec::new();
        let untouched = with_pulse_every(&mut out, QUICK, |pulsed| {
            pulsed.write_all(b"{}\n").unwrap();
            unless_abandoned(Helper::Probe(stop.clone(), "stopped"), || {
                pause(QUICK * 4);
                "finished"
            })
        });
        assert_eq!(untouched, "finished");
        assert_eq!(
            unless_abandoned(Helper::Probe(stop.clone(), "stopped"), || 3),
            3
        );
        stopped.try_recv().unwrap_err();

        with_pulse_every(&mut Hungup, BEAT, |pulsed| {
            unless_abandoned(Helper::Probe(stop.clone(), "outer"), || {
                unless_abandoned(Helper::Probe(stop, "inner"), || {});
                pulsed.write_all(b"{}\n").unwrap_err();
            });
        });
        assert_eq!(stopped.try_iter().collect::<Vec<_>>(), ["outer"]);
    }

    struct Broken;

    impl std::io::Read for Broken {
        fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::ConnectionReset.into())
        }
    }

    #[test]
    fn a_failed_read_is_passed_on_and_not_counted_as_hearing_anything() {
        let activity = Activity::default();
        let mut counted = Counted::new(Broken, activity.clone());
        assert_eq!(
            std::io::Read::read(&mut counted, &mut [0; 4])
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::ConnectionReset
        );
        assert_eq!(activity.seen(), (0, 0));
    }

    #[test]
    fn a_slow_answer_is_preceded_by_blank_lines_and_followed_by_nothing() {
        let mut out = Vec::new();
        with_pulse_every(&mut out, QUICK, |pulsed| {
            pause(QUICK * 6);
            pulsed.write_all(b"{\"reply\":\"job\"}\n").unwrap();
            pause(QUICK * 6);
        });
        let text = String::from_utf8(out).unwrap();
        let (before, after) = text.split_once("{\"reply\"").unwrap();
        assert!(
            !before.is_empty() && before.chars().all(|c| c == '\n'),
            "{before:?}"
        );
        assert_eq!(after, ":\"job\"}\n");
    }

    #[test]
    fn a_stream_is_kept_alive_with_reserved_chunks_after_its_reply() {
        let mut out = Vec::new();
        with_pulse_every(&mut out, QUICK, |pulsed| {
            pulsed.write_all(STREAM_REPLY).unwrap();
            pulsed.write_all(b"\n").unwrap();
            pause(QUICK * 6);
        });
        let rest = out.get(STREAM_REPLY.len().saturating_add(1)..).unwrap();
        assert!(!rest.is_empty());
        assert!(rest.chunks(4).all(|beat| beat == STREAM_BEAT.to_be_bytes()));
    }

    #[derive(Debug)]
    struct Refusing;

    impl Write for Refusing {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::Error::other("refused"))
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other("refused"))
        }
    }

    #[test]
    fn a_failing_output_is_reported_through_the_pulse() {
        let mut out = Refusing;
        with_pulse(&mut out, |pulsed| {
            pulsed.write_all(b"x").unwrap_err();
            pulsed.flush().unwrap_err();
        });
    }

    #[test]
    fn counted_streams_pass_bytes_through_and_count_only_real_traffic() {
        let activity = Activity::default();
        let mut reader = Counted::new(&b"abc"[..], activity.clone());
        let mut got = [0u8; 8];
        assert_eq!(std::io::Read::read(&mut reader, &mut got).unwrap(), 3);
        assert_eq!(activity.seen(), (0, 1));
        assert_eq!(std::io::Read::read(&mut reader, &mut got).unwrap(), 0);
        assert_eq!(activity.seen(), (0, 1));
        let mut writer = Counted::new(Vec::new(), activity.clone());
        assert_eq!(writer.write(b"de").unwrap(), 2);
        assert_eq!(activity.seen(), (1, 1));
        assert_eq!(writer.write(b"").unwrap(), 0);
        assert_eq!(activity.seen(), (1, 1));
        writer.flush().unwrap();
        assert_eq!(writer.inner, b"de");
        let mut refusing = Counted::new(Refusing, activity.clone());
        refusing.write_all(b"x").unwrap_err();
        refusing.flush().unwrap_err();
        assert_eq!(activity.seen(), (1, 1));
    }

    fn watch(activity: &Activity, limits: Limits) -> (Watchdog, std::sync::mpsc::Receiver<()>) {
        let (told, heard) = std::sync::mpsc::channel();
        let dog = Watchdog::guard(activity, limits, move || match told.send(()) {
            Ok(()) | Err(_) => {}
        });
        (dog, heard)
    }

    #[test]
    fn sending_alone_is_not_an_answer_so_the_first_word_limit_applies() {
        let limits = Limits {
            first_word: Duration::from_millis(400),
            silence: Duration::from_millis(40),
        };
        let activity = Activity::default();
        let started = Instant::now();
        let (dog, heard) = watch(&activity, limits);
        pause(QUICK);
        activity.sent();
        heard.recv().unwrap();
        assert!(started.elapsed() >= limits.first_word);
        assert!(dog.silenced());
    }

    #[test]
    fn after_the_first_answer_the_shorter_silence_limit_applies() {
        let limits = Limits {
            first_word: Duration::from_secs(30),
            silence: Duration::from_millis(40),
        };
        let activity = Activity::default();
        let started = Instant::now();
        let (dog, heard) = watch(&activity, limits);
        pause(QUICK);
        activity.touch();
        heard.recv().unwrap();
        assert!(started.elapsed() < limits.first_word);
        assert!(dog.silenced());
    }

    #[test]
    fn a_watchdog_dropped_early_stops_at_once_without_silencing() {
        let limits = Limits {
            first_word: Duration::from_secs(30),
            silence: Duration::from_secs(30),
        };
        let activity = Activity::default();
        let (dog, heard) = watch(&activity, limits);
        assert!(!dog.silenced());
        let started = Instant::now();
        drop(dog);
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(heard.try_recv().is_err());
    }

    #[test]
    fn a_talking_peer_is_never_silenced() {
        let limits = Limits {
            first_word: Duration::from_millis(60),
            silence: Duration::from_millis(60),
        };
        let talking = Activity::default();
        let (dog, heard) = watch(&talking, limits);
        for _ in 0..20 {
            talking.touch();
            pause(Duration::from_millis(10));
        }
        assert!(heard.try_recv().is_err());
        assert!(!dog.silenced());
    }
}
