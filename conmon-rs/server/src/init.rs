use anyhow::{bail, Result};
use libc::{c_char, c_int, setlocale, LC_ALL};
use log::info;
use std::{
    ffi::CString,
    fs::File,
    io::{self, ErrorKind, Write},
    path::Path,
};

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
                    info!("Missing sufficient privileges to adjust OOM score")
                }
                _ => bail!("adjusting OOM score {}", err),
            }
        }
        Ok(())
    }
}

#[cfg_attr(test, automock)]
pub trait InitImpl {
    fn setlocale(&self, category: c_int, locale: *const c_char) -> *mut c_char;
    fn create_file<P: 'static + AsRef<Path>>(&self, path: P) -> io::Result<File>;
    fn write_all_file(&self, file: &mut File, buf: &[u8]) -> io::Result<()>;
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
            .returning(|_: &str| Err(io::Error::new(ErrorKind::Other, "")));

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
            .returning(|_, _| Err(io::Error::new(ErrorKind::Other, "")));

        let sut = new_sut(mock);
        let res = sut.set_oom_score("-1000");

        assert!(res.is_err());
    }
}
