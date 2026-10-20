use anyhow::{Result, bail};
use libc::{LC_ALL, c_char, c_int, setlocale};
use nix::sys::stat::{Mode, umask};
use std::{
    env,
    ffi::CString,
    fs::File,
    io::{self, ErrorKind, Write},
    path::Path,
    sync::OnceLock,
};
use tokio::process::Command;

#[cfg(test)]
use mockall::{automock, predicate::*};

#[derive(Debug, Default)]
pub struct Init<T> {
    imp: T,
}

impl<T> Init<T>
where
    T: InitImpl,
{
    /// Unset the locale for the current process.
    pub fn unset_locale(&self) -> Result<()> {
        self.imp.setlocale(LC_ALL, CString::new("")?.as_ptr());
        Ok(())
    }

    /// Helper to adjust the OOM score of the currently running process.
    pub fn set_oom_score<S: AsRef<str>>(&self, score: S) -> Result<()> {
        // Attempt adjustment with best-effort.
        let mut file = self.imp.create_file("/proc/self/oom_score_adj")?;
        if let Err(err) = self
            .imp
            .write_all_file(&mut file, score.as_ref().as_bytes())
        {
            match err.kind() {
                ErrorKind::PermissionDenied => {
                    eprintln!("Missing sufficient privileges to adjust OOM score")
                }
                _ => bail!("adjusting OOM score {}", err),
            }
        }
        Ok(())
    }

    /// Set the default umask
    pub fn set_default_umask(&self) {
        self.imp.umask(Mode::from_bits_truncate(0o022))
    }

    /// Drop the sd_notify socket of whoever started the server. The server
    /// does not talk to systemd itself, but every process it runs would
    /// inherit the socket: the runtime then adds its own notify socket mount
    /// and `NOTIFY_SOCKET` to the container, the cleanup commands and the
    /// pause process get it as well. Callers set those up themselves and let
    /// the server relay the messages, see the `notify` module.
    pub fn unset_notify_socket(&self) {
        self.imp.remove_var("NOTIFY_SOCKET")
    }
}

/// The OOM score adjustment the server started with, see [`reset_oom_score`].
static INHERITED_OOM_SCORE: OnceLock<Box<[u8]>> = OnceLock::new();

/// Remember the OOM score adjustment the server started with, before it
/// protects itself with [`Init::set_oom_score`].
pub fn save_oom_score() {
    match std::fs::read("/proc/self/oom_score_adj") {
        Ok(score) => {
            let _ = INHERITED_OOM_SCORE.set(score.trim_ascii().into());
        }
        Err(e) => eprintln!("Unable to read OOM score: {e}"),
    }
}

/// Give a child the OOM score adjustment the server started with instead of
/// the one it protects itself with. The runtimes pass theirs on to the
/// container processes, an exec would be the last process the kernel picks
/// otherwise. conmon resets its runtime the same way. Only for the runtimes
/// and cleanup commands, the pause process keeps the one of the server.
pub fn reset_oom_score(cmd: &mut Command) {
    let Some(score) = INHERITED_OOM_SCORE.get() else {
        return;
    };
    let score: &'static [u8] = score;
    // SAFETY: the hook allocates nothing and only calls open, write and
    // close, which are async signal safe. Raising the score needs no
    // privileges, a failure keeps the inherited one, as before.
    unsafe {
        cmd.pre_exec(move || {
            let fd = libc::open(
                c"/proc/self/oom_score_adj".as_ptr(),
                libc::O_WRONLY | libc::O_CLOEXEC,
            );
            if fd >= 0 {
                libc::write(fd, score.as_ptr().cast(), score.len());
                libc::close(fd);
            }
            Ok(())
        });
    }
}

#[cfg_attr(test, automock)]
pub trait InitImpl {
    fn setlocale(&self, category: c_int, locale: *const c_char) -> *mut c_char;
    fn create_file<P: 'static + AsRef<Path>>(&self, path: P) -> io::Result<File>;
    fn write_all_file(&self, file: &mut File, buf: &[u8]) -> io::Result<()>;
    fn umask(&self, mode: Mode);
    fn remove_var(&self, key: &str);
}

#[derive(Debug, Default)]
pub struct DefaultInit;

impl InitImpl for DefaultInit {
    fn setlocale(&self, category: c_int, locale: *const c_char) -> *mut c_char {
        unsafe { setlocale(category, locale) }
    }

    fn create_file<P: AsRef<Path>>(&self, path: P) -> io::Result<File> {
        File::create(path)
    }

    fn write_all_file(&self, file: &mut File, buf: &[u8]) -> io::Result<()> {
        file.write_all(buf)
    }

    fn umask(&self, mode: Mode) {
        umask(mode);
    }

    fn remove_var(&self, key: &str) {
        // SAFETY: the server is still single threaded, the runtime and its
        // threads start later.
        unsafe { env::remove_var(key) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{ptr, str};
    use tempfile::tempfile;

    fn new_sut(mock: MockInitImpl) -> Init<MockInitImpl> {
        Init::<MockInitImpl> { imp: mock }
    }

    #[test]
    fn unset_locale() -> Result<()> {
        let mut mock = MockInitImpl::new();
        mock.expect_setlocale()
            .withf(|x, _| *x == LC_ALL)
            .returning(|_, _| ptr::null_mut());

        let sut = new_sut(mock);

        sut.unset_locale()
    }

    #[test]
    fn set_oom_success() -> Result<()> {
        let mut mock = MockInitImpl::new();

        mock.expect_create_file()
            .with(eq("/proc/self/oom_score_adj"))
            .returning(|_: &str| tempfile());

        mock.expect_write_all_file()
            .withf(|_, x| x == "-1000".as_bytes())
            .returning(|_, _| Ok(()));

        let sut = new_sut(mock);
        sut.set_oom_score("-1000")
    }

    #[test]
    fn set_oom_success_write_all_fails_permission_denied() -> Result<()> {
        let mut mock = MockInitImpl::new();

        mock.expect_create_file()
            .with(eq("/proc/self/oom_score_adj"))
            .returning(|_: &str| tempfile());

        mock.expect_write_all_file()
            .withf(|_, x| x == "-1000".as_bytes())
            .returning(|_, _| Err(io::Error::new(ErrorKind::PermissionDenied, "")));

        let sut = new_sut(mock);
        sut.set_oom_score("-1000")
    }

    #[test]
    fn set_oom_failed_create_file() {
        let mut mock = MockInitImpl::new();

        mock.expect_create_file()
            .with(eq("/proc/self/oom_score_adj"))
            .returning(|_: &str| Err(io::Error::other("")));

        let sut = new_sut(mock);
        let res = sut.set_oom_score("-1000");

        assert!(res.is_err());
    }

    #[test]
    fn set_oom_failed_write_all_file() {
        let mut mock = MockInitImpl::new();

        mock.expect_create_file()
            .with(eq("/proc/self/oom_score_adj"))
            .returning(|_: &str| tempfile());

        mock.expect_write_all_file()
            .withf(|_, x| x == "-1000".as_bytes())
            .returning(|_, _| Err(io::Error::other("")));

        let sut = new_sut(mock);
        let res = sut.set_oom_score("-1000");

        assert!(res.is_err());
    }

    #[test]
    fn umask() {
        let mut mock = MockInitImpl::new();
        mock.expect_umask().returning(|_| ());

        let sut = new_sut(mock);
        sut.set_default_umask();
    }
}
