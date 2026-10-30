//! The logger which writes the container output to journald.

use crate::{
    container_io::Pipe,
    container_log::{Line, Logger},
    journal::{self, Entry, Priority, Wait},
};
use anyhow::{Context, Result, bail};
use std::{
    io::{self, ErrorKind},
    path::Path,
    sync::{
        Arc,
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
pub struct JournaldLogger {
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
    /// A temporary socket instead of the one of journald.
    #[cfg(test)]
    socket: Option<std::path::PathBuf>,
}

impl Shared {
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

    /// The fields the logger sets itself, which a label cannot replace.
    const OWN_FIELDS: [&'static str; 8] = [
        "MESSAGE",
        "PRIORITY",
        "CONTAINER_ID",
        "CONTAINER_ID_FULL",
        "CONTAINER_NAME",
        "CONTAINER_TAG",
        "SYSLOG_IDENTIFIER",
        Self::PARTIAL_FIELD.0,
    ];

    /// The most labels of a logger. journald drops entries with more than
    /// 1024 fields (`ENTRY_FIELDS_MAX`), which would lose every line.
    const MAX_LABELS: usize = 256;

    /// The most bytes of the names and values of all labels together, which
    /// every entry carries.
    const MAX_LABEL_BYTES: usize = 64 * 1024;

    /// A logger with the fields conmon writes: the container ID, the name
    /// and the tag if given, the syslog identifier and the labels. Like for
    /// conmon, the syslog identifier is the tag, the name without one, and
    /// the short container ID without both.
    pub fn new<'a>(
        id: &str,
        name: &str,
        tag: &str,
        labels: impl IntoIterator<Item = (&'a str, &'a str)>,
    ) -> Result<Self> {
        let short_id: String = id.chars().take(Self::SHORT_ID_LEN).collect();
        let mut fields = Entry::default();
        fields
            .field("CONTAINER_ID_FULL", id)
            .field("CONTAINER_ID", &short_id);
        if !name.is_empty() {
            fields.field("CONTAINER_NAME", name);
        }
        if !tag.is_empty() {
            fields.field("CONTAINER_TAG", tag);
        }
        let identifier = [tag, name, &short_id]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or(journal::IDENTIFIER);
        fields.field("SYSLOG_IDENTIFIER", identifier);
        let (mut count, mut bytes) = (0, 0);
        for (key, value) in labels {
            Self::validate_label(key)?;
            count += 1;
            bytes += key.len() + value.len();
            if count > Self::MAX_LABELS || bytes > Self::MAX_LABEL_BYTES {
                bail!(
                    "too many journald labels: at most {} with {} bytes together",
                    Self::MAX_LABELS,
                    Self::MAX_LABEL_BYTES
                );
            }
            fields.field(key, value);
        }
        Ok(Self {
            shared: Arc::new(Shared {
                fields,
                failing: AtomicBool::new(false),
                #[cfg(test)]
                socket: None,
            }),
        })
    }

    /// Check that a label is a field name which the journal takes and which
    /// the logger does not set itself.
    fn validate_label(key: &str) -> Result<()> {
        if !Entry::valid_key(key) {
            bail!(
                "invalid journald label {key:?}: use uppercase letters, digits and \
                 underscores, starting with a letter, at most 64 characters"
            );
        }
        if Self::OWN_FIELDS.contains(&key) {
            bail!("journald label {key:?} replaces a field of the logger");
        }
        Ok(())
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
            let rest = journal::unavailable(&e) || e.kind() == ErrorKind::TimedOut;
            last_error = Some(e);
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

impl Logger for JournaldLogger {
    const BUFFERS_LINES: bool = true;

    /// A journal which keeps up takes the lines right away. The ones it does
    /// not take are sent from a single blocking task, which waits for it a
    /// bounded time, instead of one per line. The lines which borrow the
    /// chunk get copied for it, the others share their buffer.
    async fn write_lines(&mut self, pipe: Pipe, lines: &[Line<'_>]) -> Result<()> {
        let sent = Self::send(&self.shared, pipe, lines, Wait::Never);
        let Some(rest) = lines.get(sent..).filter(|rest| !rest.is_empty()) else {
            return Ok(());
        };
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
