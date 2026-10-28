use super::*;
use crate::process::child_pid;
use std::{
    os::unix::process::ExitStatusExt,
    sync::{Arc, atomic::AtomicUsize},
};
use tokio::{process::Command, sync::Notify};

/// A value which counts how often it got stopped.
struct Value(Arc<AtomicUsize>);

impl Stop for Value {
    fn stop(&self) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

fn flight() -> &'static SingleFlight<Value> {
    Box::leak(Box::new(SingleFlight::new()))
}

/// An initialization which counts its runs and takes a second.
fn slow_init(
    inits: &Arc<AtomicUsize>,
    stops: &Arc<AtomicUsize>,
) -> impl FnOnce() -> std::pin::Pin<Box<dyn Future<Output = Result<Value>> + Send>> + Send + 'static
{
    let (inits, stops) = (inits.clone(), stops.clone());
    move || {
        Box::pin(async move {
            inits.fetch_add(1, Ordering::SeqCst);
            time::sleep(Duration::from_secs(1)).await;
            Ok(Value(stops))
        })
    }
}

#[tokio::test(start_paused = true)]
async fn concurrent_callers_init_once() -> Result<()> {
    let flight = flight();
    let (inits, stops) = (Arc::default(), Arc::default());

    let (a, b, c) = tokio::join!(
        flight.get_or_init(slow_init(&inits, &stops)),
        flight.get_or_init(slow_init(&inits, &stops)),
        flight.get_or_init(slow_init(&inits, &stops)),
    );
    let (a, b, c) = (a?, b?, c?);
    assert!(std::ptr::eq(a, b) && std::ptr::eq(b, c));
    assert_eq!(inits.load(Ordering::SeqCst), 1);

    // Later callers get the value right away.
    flight.get_or_init(slow_init(&inits, &stops)).await?;
    assert_eq!(inits.load(Ordering::SeqCst), 1);
    assert_eq!(stops.load(Ordering::SeqCst), 0);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn failed_init_allows_retry() -> Result<()> {
    let flight = flight();
    let (inits, stops) = (Arc::default(), Arc::default());

    let failing = || async {
        time::sleep(Duration::from_secs(1)).await;
        bail!("init failed")
    };
    let (first, waiter) = tokio::join!(
        flight.get_or_init(failing),
        flight.get_or_init(slow_init(&inits, &stops)),
    );
    assert!(format!("{:#}", first.err().context("no error")?).contains("init failed"));
    // The waiter does not run its own, it gets the failure.
    assert!(waiter.is_err());
    assert_eq!(inits.load(Ordering::SeqCst), 0);

    flight.get_or_init(slow_init(&inits, &stops)).await?;
    assert_eq!(inits.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn cancelled_caller_does_not_abandon_init() -> Result<()> {
    let flight = flight();
    let (inits, stops) = (Arc::default(), Arc::default());

    let cancelled = time::timeout(
        Duration::from_millis(100),
        flight.get_or_init(slow_init(&inits, &stops)),
    )
    .await;
    assert!(cancelled.is_err());

    // The initialization went on, the next caller waits for it.
    flight.get_or_init(slow_init(&inits, &stops)).await?;
    assert_eq!(inits.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn shutdown_waits_for_running_init() -> Result<()> {
    let flight = flight();
    let (inits, stops) = (Arc::default(), Arc::<AtomicUsize>::default());

    let caller = task::spawn(flight.get_or_init(slow_init(&inits, &stops)));
    while !*flight.running.borrow() {
        task::yield_now().await;
    }

    let start = time::Instant::now();
    flight
        .shutdown(
            start + Duration::from_secs(12),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .await?;
    assert_eq!(start.elapsed(), Duration::from_secs(1));

    // It finished after the shutdown started, so it stopped its value
    // instead of publishing it.
    assert!(caller.await?.is_err());
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    assert!(flight.cell.get().is_none());

    // Nothing starts after the shutdown.
    assert!(flight.get_or_init(slow_init(&inits, &stops)).await.is_err());
    assert_eq!(inits.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn shutdown_gives_up_on_hanging_init() -> Result<()> {
    let flight = flight();
    let stops = Arc::<AtomicUsize>::default();
    let release = Arc::new(Notify::new());

    let caller = task::spawn(flight.get_or_init({
        let (stops, release) = (stops.clone(), release.clone());
        move || async move {
            release.notified().await;
            Ok(Value(stops))
        }
    }));
    while !*flight.running.borrow() {
        task::yield_now().await;
    }

    let start = time::Instant::now();
    let err = flight
        .shutdown(
            start + Duration::from_secs(12),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .await
        .err()
        .map(|e| e.to_string());
    assert_eq!(start.elapsed(), Duration::from_secs(11));
    assert_eq!(
        err.as_deref(),
        Some("initialization did not finish, its process may stay behind")
    );
    assert_eq!(stops.load(Ordering::SeqCst), 0);

    // Once it finishes late it stops its value itself.
    release.notify_one();
    assert!(caller.await?.is_err());
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    assert!(flight.cell.get().is_none());
    Ok(())
}

#[tokio::test]
async fn shutdown_kills_init_process() -> Result<()> {
    let flight = flight();
    let stops = Arc::<AtomicUsize>::default();

    let caller = task::spawn(flight.get_or_init({
        let stops = stops.clone();
        move || async move {
            let mut child = Command::new("sleep").arg("60").kill_on_drop(true).spawn()?;
            let pid = child_pid(child.id())?;
            flight.set_child(Some(Process::open(pid)));
            let status = child.wait().await?;
            flight.set_child(None);
            assert_eq!(status.signal(), Some(Signal::SIGKILL as i32));
            Ok(Value(stops))
        }
    }));
    while lock(&flight.child).is_none() {
        task::yield_now().await;
    }

    flight
        .shutdown(
            time::Instant::now() + Duration::from_millis(11100),
            Duration::from_secs(10),
            Duration::from_secs(1),
        )
        .await?;
    assert!(caller.await?.is_err());
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(start_paused = true)]
async fn shutdown_stops_initialized_value() -> Result<()> {
    let flight = flight();
    let (inits, stops) = (Arc::default(), Arc::<AtomicUsize>::default());
    flight.get_or_init(slow_init(&inits, &stops)).await?;

    flight
        .shutdown(
            time::Instant::now() + Duration::from_secs(12),
            Duration::from_secs(1),
            Duration::from_secs(1),
        )
        .await?;
    assert_eq!(stops.load(Ordering::SeqCst), 1);
    Ok(())
}

/// A value whose stop hangs until released, like an unmount on a hung
/// file system, and which tells the thread it stopped on.
struct Hung {
    release: Mutex<std::sync::mpsc::Receiver<()>>,
    stopped_on: Mutex<Option<std::thread::ThreadId>>,
}

impl Stop for Hung {
    fn stop(&self) {
        if let Ok(mut stopped_on) = self.stopped_on.lock() {
            *stopped_on = Some(std::thread::current().id());
        }
        if let Ok(release) = self.release.lock() {
            let _ = release.recv();
        }
    }
}

#[tokio::test]
async fn shutdown_does_not_wait_for_a_hung_stop() -> Result<()> {
    let flight: &'static SingleFlight<Hung> = Box::leak(Box::new(SingleFlight::new()));
    let (release, rx) = std::sync::mpsc::channel();
    let value = flight
        .get_or_init(move || async move {
            Ok(Hung {
                release: Mutex::new(rx),
                stopped_on: Mutex::default(),
            })
        })
        .await?;

    // The runtime thread keeps running while the stop hangs.
    let ticker = task::spawn(async {
        for _ in 0..5 {
            time::sleep(Duration::from_millis(20)).await;
        }
    });
    let start = time::Instant::now();
    let res = flight
        .shutdown(
            start + Duration::from_millis(200),
            Duration::from_secs(1),
            Duration::from_millis(200),
        )
        .await;
    let elapsed = start.elapsed();
    assert_eq!(
        res.err().map(|e| e.to_string()).as_deref(),
        Some("stop did not finish in time")
    );
    assert!(elapsed >= Duration::from_millis(200), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(5), "{elapsed:?}");
    assert!(ticker.is_finished());
    let stopped_on = *lock(&value.stopped_on);
    assert!(stopped_on.is_some_and(|id| id != std::thread::current().id()));

    // Let the blocking pool end with the test.
    release.send(())?;
    Ok(())
}
