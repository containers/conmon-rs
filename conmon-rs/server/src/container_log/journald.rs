//! The logger which writes the container output to journald.

use crate::{
    container_io::Pipe,
    container_log::Line,
    journal::{self, Entry, Priority, Wait},
    mutex::lock,
};
use anyhow::{Context, Result};
use std::{
    io::{self, ErrorKind},
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::task;
use tracing::{debug, info, warn};

/// Writes the container output to the journal with the container ID fields
/// and priorities conmon uses, so that `journalctl CONTAINER_ID=<id>` finds
/// it and podman can put partial lines back together.
#[derive(Debug)]
pub(super) struct JournaldLogger {
    /// Shared with the blocking tasks which send the entries.
    shared: Arc<Shared>,
}

#[derive(Debug)]
struct Shared {
    /// The fields of every entry, encoded once.
    fields: Entry,
    /// Whether the last send failed, so that a journal which is unavailable
    /// gets reported once and not for every chunk of output.
    failing: AtomicBool,
    /// When the lines which last waited for the journal did not go out in
    /// time. The following chunks drop what the journal does not take right
    /// away instead of waiting as well, for at least another
    /// `CHUNK_TIMEOUT` and after that as long as it takes nothing, like a
    /// stalled one. Waiting for every chunk would slow down the reading of
    /// the output to about one per `CHUNK_TIMEOUT`, which the other log
    /// drivers and the container, blocked on a full pipe, would wait for.
    /// This way a journal which does not keep up holds it back by about half
    /// at most, and one which stalled not at all.
    timed_out: Mutex<Option<Instant>>,
    /// A temporary socket instead of the one of journald.
    #[cfg(test)]
    socket: Option<std::path::PathBuf>,
}

impl Shared {
    /// Whether the lines which the journal did not take right away get
    /// dropped instead of waiting for it, after `sent` lines went out
    /// without waiting, see [`Shared::timed_out`].
    fn skip_wait(&self, sent: usize) -> bool {
        let mut timed_out = lock(&self.timed_out);
        match *timed_out {
            Some(at) if sent == 0 || at.elapsed() < CHUNK_TIMEOUT => true,
            _ => {
                *timed_out = None;
                false
            }
        }
    }

    fn socket(&self) -> &Path {
        #[cfg(test)]
        if let Some(socket) = &self.socket {
            return socket;
        }
        Path::new(journal::SOCKET_PATH)
    }
}

impl JournaldLogger {
    /// The length of the short container ID field.
    const SHORT_ID_LEN: usize = 12;

    /// The field marking a line which had no ending yet.
    const PARTIAL_FIELD: (&'static str, &'static str) = ("CONTAINER_PARTIAL_MESSAGE", "true");

    /// A logger with the fields conmon writes: the container ID, the name
    /// if given and the syslog identifier, which is the name, or the short
    /// container ID without one, like for conmon without a tag.
    pub(super) fn new(id: &str, name: &str) -> Self {
        let short_id: String = id.chars().take(Self::SHORT_ID_LEN).collect();
        let mut fields = Entry::default();
        fields
            .field("CONTAINER_ID_FULL", id)
            .field("CONTAINER_ID", &short_id);
        if !name.is_empty() {
            fields.field("CONTAINER_NAME", name);
        }
        let identifier = [name, &short_id]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or(journal::IDENTIFIER);
        fields.field("SYSLOG_IDENTIFIER", identifier);
        Self {
            shared: Arc::new(Shared {
                fields,
                failing: AtomicBool::new(false),
                timed_out: Mutex::new(None),
                #[cfg(test)]
                socket: None,
            }),
        }
    }

    /// A logger which sends to the `socket` instead of the one of journald.
    #[cfg(test)]
    pub(super) fn with_socket(id: &str, socket: &Path) -> Self {
        let mut logger = Self::new(id, "");
        Arc::get_mut(&mut logger.shared)
            .expect("unshared logger")
            .socket = Some(socket.to_owned());
        logger
    }

    /// The journal priority of the output of a pipe.
    fn priority(pipe: Pipe) -> Priority {
        match pipe {
            Pipe::StdOut => Priority::Info,
            Pipe::StdErr => Priority::Error,
        }
    }

    /// Send the lines to the journal and return how many of them it took
    /// or got dropped. A journal which is unavailable, for example while it
    /// restarts, must not end the output of the container, so failures are
    /// only logged, once until a send succeeds again. Without a journal, or
    /// with one which did not take a line in time, the rest of the chunk is
    /// dropped right away instead of failing the same way line by line.
    /// Without waiting, the sending stops at the first line which the
    /// journal does not take right away. Waiting, the lines which did not
    /// go out by the deadline of the wait get dropped, a journal which takes
    /// each line just in time would block the log of the container for
    /// minutes per chunk otherwise.
    fn send(shared: &Shared, pipe: Pipe, lines: &[Line<'_>], wait: Wait) -> usize {
        let priority = Self::priority(pipe);
        let mut entry = Entry::default();
        let mut handled = lines.len();
        let mut failed = 0;
        let mut last_error = None;
        for (i, line) in lines.iter().enumerate() {
            if let Wait::Bounded(deadline) = wait
                && Instant::now() >= deadline
            {
                failed += lines.len() - i;
                *lock(&shared.timed_out) = Some(Instant::now());
                last_error = Some(io::Error::new(
                    ErrorKind::TimedOut,
                    format!("journal did not take the chunk within {CHUNK_TIMEOUT:?}"),
                ));
                break;
            }
            entry.reset(priority, &*line.text).extend(&shared.fields);
            if line.partial {
                entry.field(Self::PARTIAL_FIELD.0, Self::PARTIAL_FIELD.1);
            }
            let Err(e) = journal::send_at(wait, shared.socket(), &entry) else {
                continue;
            };
            if wait == Wait::Never && e.kind() == ErrorKind::WouldBlock {
                handled = i;
                break;
            }
            let timed_out = e.kind() == ErrorKind::TimedOut;
            let rest = timed_out || journal::unavailable(&e);
            last_error = Some(e);
            if timed_out {
                *lock(&shared.timed_out) = Some(Instant::now());
            }
            if rest {
                failed += lines.len() - i;
                break;
            }
            failed += 1;
        }
        match last_error {
            Some(e) if !shared.failing.swap(true, Ordering::Relaxed) => warn!(
                "Unable to write {failed} lines to the journal, dropping lines \
                 until it takes them again: {e}"
            ),
            Some(e) => debug!("Unable to write {failed} lines to the journal: {e}"),
            // Only once the whole chunk went out, not some lines before the
            // journal stalled again.
            None if handled == lines.len() && shared.failing.swap(false, Ordering::Relaxed) => {
                info!("The journal takes lines again");
            }
            None => {}
        }
        handled
    }
}

/// How long the lines of a chunk which the journal did not take right away
/// may wait for it in total. A line which started before it may take one
/// more `journal::SEND_TIMEOUT`, sends which a signal interrupted get
/// retried only until it.
const CHUNK_TIMEOUT: Duration = Duration::from_secs(1);

impl JournaldLogger {
    /// Write whole lines of output of the pipe, one entry each. A journal
    /// which keeps up takes the lines right away. The ones it does not take
    /// are sent from a single blocking task, which waits for it a bounded
    /// time, instead of one per line, unless the last wait timed out, see
    /// [`Shared::timed_out`]. The lines which borrow the chunk get copied for
    /// it, the others share their buffer.
    pub(super) async fn write_lines(&self, pipe: Pipe, lines: &[Line<'_>]) -> Result<()> {
        let sent = Self::send(&self.shared, pipe, lines, Wait::Never);
        let Some(rest) = lines.get(sent..).filter(|rest| !rest.is_empty()) else {
            return Ok(());
        };
        if self.shared.skip_wait(sent) {
            // The timeout got logged already, see `Shared::failing`.
            debug!(
                "Unable to write {} lines to the journal: it did not take the last ones in time",
                rest.len()
            );
            return Ok(());
        }
        let shared = self.shared.clone();
        let rest: Vec<_> = rest.iter().cloned().map(Line::into_owned).collect();
        // From when a thread of the blocking pool took it, a busy pool must
        // not drop the chunk without trying.
        task::spawn_blocking(move || {
            let deadline = Instant::now() + CHUNK_TIMEOUT;
            Self::send(&shared, pipe, &rest, Wait::Bounded(deadline))
        })
        .await
        .map(drop)
        .context("journal spawn_blocking")
    }
}

#[cfg(test)]
mod tests;
