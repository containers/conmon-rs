use crate::bpf::{pidwatch_bss_types::event, PidwatchSkelBuilder};
use anyhow::{bail, format_err, Context, Error, Result};
use libbpf_rs::RingBufferBuilder;
use nix::sys::resource::{setrlimit, Resource};
use plain::Plain;
use std::time::Duration;
use tokio::{
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
    task,
};
use tracing::{debug, debug_span, error, Instrument};

unsafe impl Plain for event {}

#[derive(Debug)]
/// An event send by the `PidWatch` instance.
pub enum Event {
    /// An error indicating that the PID watcher failed.
    Err(Error),

    /// A process exited normally.
    Exited(i32),

    /// A signal stopped the process.
    Signaled(i32),

    /// The process got killed because it ran out of memory.
    OOMKilled,
}

#[derive(Debug)]
/// The main PID watching type of this module.
pub struct PidWatch {
    pid: u32,
}

impl PidWatch {
    /// Create a new PidWatch instance.
    pub fn new(pid: u32) -> Self {
        debug!("Initializing new PID watcher for PID: {pid}");
        Self { pid }
    }

    /// Run the PID watcher.
    pub async fn run(&self) -> Result<UnboundedReceiver<Event>> {
        debug!("Running PID watcher");
        let skel_builder = PidwatchSkelBuilder::default();

        Self::set_memlock_rlimit().context("bump memlock rlimit")?;
        let mut open_skel = skel_builder.open().context("open skel builder")?;

        open_skel.rodata().cfg.pid = self.pid;

        let mut skel = open_skel.load().context("load skel")?;
        skel.attach().context("attach skel")?;

        let (tx, rx) = mpsc::unbounded_channel();
        let (stop_tx, mut stop_rx) = mpsc::unbounded_channel();

        task::spawn(
            async move {
                let mut ringbuffer_builder = RingBufferBuilder::new();
                if let Err(e) = ringbuffer_builder
                    .add(skel.maps_mut().ringbuf(), |data| {
                        Self::callback(data, &tx, &stop_tx)
                    })
                    .context("add ringbuffer callback")
                {
                    tx.send(Event::Err(e)).expect("send error event");
                    return;
                }

                match ringbuffer_builder.build().context("build ringbuffer") {
                    Err(e) => tx.send(Event::Err(e)).expect("send error event"),
                    Ok(ringbuffer) => loop {
                        if stop_rx.try_recv().is_ok() {
                            debug!("Stopping ringbuffer loop");
                            break;
                        }

                        if let Err(e) = ringbuffer
                            .poll(Duration::from_secs(1))
                            .context("unable to poll from ringbuffer")
                        {
                            error!("{:#}", e);
                            tx.send(Event::Err(e)).expect("send error event");
                            break;
                        }
                    },
                };
            }
            .instrument(debug_span!("ringbuffer")),
        );

        Ok(rx)
    }

    fn callback(data: &[u8], tx: &UnboundedSender<Event>, stop_tx: &UnboundedSender<()>) -> i32 {
        if let Err(e) = Self::handle_event(data, tx, stop_tx) {
            error!("Unable to handle event: {:#}", e);
            tx.send(Event::Err(e)).expect("send error event");
            stop_tx.send(()).expect("send stop message");
        }

        // We just need one callback from the ebpf application, stop here.
        1
    }

    fn handle_event(
        data: &[u8],
        tx: &UnboundedSender<Event>,
        stop_tx: &UnboundedSender<()>,
    ) -> Result<()> {
        let mut event = event::default();
        plain::copy_from_bytes(&mut event, data)
            .map_err(|e| format_err!("data buffer was too short: {:?}", e))?;

        let event = match (event.exit_code, event.signaled_exit_code, event.oom_killed) {
            (_, _, true) => Event::OOMKilled,
            (0, s, false) if s != 0 => Event::Signaled(s),
            (e, 0, false) => Event::Exited(e),
            _ => bail!("invalid event combination: {:?}", event),
        };

        debug!("Sending data event: {:?}", event);
        tx.send(event).context("send exit event")?;
        stop_tx.send(()).context("send stop message")
    }

    fn set_memlock_rlimit() -> Result<()> {
        let limit = 128 << 20;
        setrlimit(Resource::RLIMIT_MEMLOCK, limit, limit).context("adjusting resource limits")
    }
}
