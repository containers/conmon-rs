//! Error types.

use core::array::TryFromSliceError;
use core::fmt;
use core::num::TryFromIntError;
use core::str::Utf8Error;

#[cfg(feature = "std")]
mod parse {
    use super::*;

    use core::num::ParseIntError;

    /// Unified error type for parsing a TZ string
    #[cfg_attr(docsrs, doc(cfg(feature = "std")))]
    #[non_exhaustive]
    #[derive(Debug)]
    pub enum TzStringError {
        /// UTF-8 error
        Utf8Error(Utf8Error),
        /// Integer parsing error
        ParseIntError(ParseIntError),
        /// I/O error
        IoError(std::io::Error),
        /// Invalid TZ string
        InvalidTzString(&'static str),
        /// Unsupported TZ string
        UnsupportedTzString(&'static str),
    }

    impl fmt::Display for TzStringError {
        fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
            match self {
                Self::Utf8Error(error) => error.fmt(f),
                Self::ParseIntError(error) => error.fmt(f),
                Self::IoError(error) => error.fmt(f),
                Self::InvalidTzString(error) => write!(f, "invalid TZ string: {}", error),
                Self::UnsupportedTzString(error) => write!(f, "unsupported TZ string: {}", error),
            }
        }
    }

    impl std::error::Error for TzStringError {}

    impl From<Utf8Error> for TzStringError {
        fn from(error: Utf8Error) -> Self {
            Self::Utf8Error(error)
        }
    }

    impl From<ParseIntError> for TzStringError {
        fn from(error: ParseIntError) -> Self {
            Self::ParseIntError(error)
        }
    }

    impl From<std::io::Error> for TzStringError {
        fn from(error: std::io::Error) -> Self {
            Self::IoError(error)
        }
    }

    /// Unified error type for parsing a TZif file
    #[cfg_attr(docsrs, doc(cfg(feature = "std")))]
    #[non_exhaustive]
    #[derive(Debug)]
    pub enum TzFileError {
        /// Conversion from slice to array error
        TryFromSliceError(TryFromSliceError),
        /// I/O error
        IoError(std::io::Error),
        /// Unified error for parsing a TZ string
        TzStringError(TzStringError),
        /// Invalid TZif file
        InvalidTzFile(&'static str),
        /// Unsupported TZif file
        UnsupportedTzFile(&'static str),
    }

    impl fmt::Display for TzFileError {
        fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
            match self {
                Self::TryFromSliceError(error) => error.fmt(f),
                Self::IoError(error) => error.fmt(f),
                Self::TzStringError(error) => error.fmt(f),
                Self::InvalidTzFile(error) => write!(f, "invalid TZ file: {}", error),
                Self::UnsupportedTzFile(error) => write!(f, "unsupported TZ file: {}", error),
            }
        }
    }

    impl std::error::Error for TzFileError {}

    impl From<TryFromSliceError> for TzFileError {
        fn from(error: TryFromSliceError) -> Self {
            Self::TryFromSliceError(error)
        }
    }

    impl From<std::io::Error> for TzFileError {
        fn from(error: std::io::Error) -> Self {
            Self::IoError(error)
        }
    }

    impl From<TzStringError> for TzFileError {
        fn from(error: TzStringError) -> Self {
            Self::TzStringError(error)
        }
    }
}

#[cfg(feature = "std")]
pub use parse::{TzFileError, TzStringError};

macro_rules! create_error {
    (#[$doc:meta], $name:ident) => {
        #[$doc]
        #[derive(Debug)]
        pub struct $name(
            /// Error description
            pub &'static str,
        );

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter) -> Result<(), fmt::Error> {
                self.0.fmt(f)
            }
        }

        #[cfg(feature = "std")]
        #[cfg_attr(docsrs, doc(cfg(feature = "std")))]
        impl std::error::Error for $name {}
    };
}

create_error!(#[doc = "Out of range error"], OutOfRangeError);
create_error!(#[doc = "Local time type error"], LocalTimeTypeError);
create_error!(#[doc = "Transition rule error"], TransitionRuleError);
create_error!(#[doc = "Time zone error"], TimeZoneError);
create_error!(#[doc = "Date time error"], DateTimeError);
create_error!(#[doc = "Local time type search error"], FindLocalTimeTypeError);
create_error!(#[doc = "Date time projection error"], ProjectDateTimeError);

impl From<OutOfRangeError> for ProjectDateTimeError {
    fn from(error: OutOfRangeError) -> Self {
        Self(error.0)
    }
}

impl From<FindLocalTimeTypeError> for ProjectDateTimeError {
    fn from(error: FindLocalTimeTypeError) -> Self {
        Self(error.0)
    }
}

/// Unified error type for everything in the crate
#[non_exhaustive]
#[derive(Debug)]
pub enum TzError {
    /// UTF-8 error
    Utf8Error(Utf8Error),
    /// Conversion from slice to array error
    TryFromSliceError(TryFromSliceError),
    /// I/O error
    #[cfg(feature = "std")]
    #[cfg_attr(docsrs, doc(cfg(feature = "std")))]
    IoError(std::io::Error),
    /// System time error
    #[cfg(feature = "std")]
    #[cfg_attr(docsrs, doc(cfg(feature = "std")))]
    SystemTimeError(std::time::SystemTimeError),
    /// Unified error for parsing a TZif file
    #[cfg(feature = "std")]
    #[cfg_attr(docsrs, doc(cfg(feature = "std")))]
    TzFileError(TzFileError),
    /// Unified error for parsing a TZ string
    #[cfg(feature = "std")]
    #[cfg_attr(docsrs, doc(cfg(feature = "std")))]
    TzStringError(TzStringError),
    /// Out of range error
    OutOfRangeError(OutOfRangeError),
    /// Local time type error
    LocalTimeTypeError(LocalTimeTypeError),
    /// Transition rule error
    TransitionRuleError(TransitionRuleError),
    /// Time zone error
    TimeZoneError(TimeZoneError),
    /// Date time error
    DateTimeError(DateTimeError),
    /// Local time type search error
    FindLocalTimeTypeError(FindLocalTimeTypeError),
    /// Date time projection error
    ProjectDateTimeError(ProjectDateTimeError),
}

impl fmt::Display for TzError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Self::Utf8Error(error) => error.fmt(f),
            Self::TryFromSliceError(error) => error.fmt(f),
            #[cfg(feature = "std")]
            Self::IoError(error) => error.fmt(f),
            #[cfg(feature = "std")]
            Self::SystemTimeError(error) => error.fmt(f),
            #[cfg(feature = "std")]
            Self::TzFileError(error) => error.fmt(f),
            #[cfg(feature = "std")]
            Self::TzStringError(error) => error.fmt(f),
            Self::OutOfRangeError(error) => error.fmt(f),
            Self::LocalTimeTypeError(error) => write!(f, "invalid local time type: {}", error),
            Self::TransitionRuleError(error) => write!(f, "invalid transition rule: {}", error),
            Self::TimeZoneError(error) => write!(f, "invalid time zone: {}", error),
            Self::DateTimeError(error) => write!(f, "invalid date time: {}", error),
            Self::FindLocalTimeTypeError(error) => error.fmt(f),
            Self::ProjectDateTimeError(error) => error.fmt(f),
        }
    }
}

#[cfg(feature = "std")]
#[cfg_attr(docsrs, doc(cfg(feature = "std")))]
impl std::error::Error for TzError {}

impl From<Utf8Error> for TzError {
    fn from(error: Utf8Error) -> Self {
        Self::Utf8Error(error)
    }
}

impl From<TryFromSliceError> for TzError {
    fn from(error: TryFromSliceError) -> Self {
        Self::TryFromSliceError(error)
    }
}

#[cfg(feature = "std")]
#[cfg_attr(docsrs, doc(cfg(feature = "std")))]
impl From<std::io::Error> for TzError {
    fn from(error: std::io::Error) -> Self {
        Self::IoError(error)
    }
}

#[cfg(feature = "std")]
#[cfg_attr(docsrs, doc(cfg(feature = "std")))]
impl From<std::time::SystemTimeError> for TzError {
    fn from(error: std::time::SystemTimeError) -> Self {
        Self::SystemTimeError(error)
    }
}

#[cfg(feature = "std")]
#[cfg_attr(docsrs, doc(cfg(feature = "std")))]
impl From<TzFileError> for TzError {
    fn from(error: TzFileError) -> Self {
        Self::TzFileError(error)
    }
}

#[cfg(feature = "std")]
#[cfg_attr(docsrs, doc(cfg(feature = "std")))]
impl From<TzStringError> for TzError {
    fn from(error: TzStringError) -> Self {
        Self::TzStringError(error)
    }
}

impl From<OutOfRangeError> for TzError {
    fn from(error: OutOfRangeError) -> Self {
        Self::OutOfRangeError(error)
    }
}

impl From<TryFromIntError> for TzError {
    fn from(_: TryFromIntError) -> Self {
        Self::OutOfRangeError(OutOfRangeError("out of range integer conversion"))
    }
}

impl From<LocalTimeTypeError> for TzError {
    fn from(error: LocalTimeTypeError) -> Self {
        Self::LocalTimeTypeError(error)
    }
}

impl From<TransitionRuleError> for TzError {
    fn from(error: TransitionRuleError) -> Self {
        Self::TransitionRuleError(error)
    }
}

impl From<TimeZoneError> for TzError {
    fn from(error: TimeZoneError) -> Self {
        Self::TimeZoneError(error)
    }
}

impl From<DateTimeError> for TzError {
    fn from(error: DateTimeError) -> Self {
        Self::DateTimeError(error)
    }
}

impl From<FindLocalTimeTypeError> for TzError {
    fn from(error: FindLocalTimeTypeError) -> Self {
        Self::FindLocalTimeTypeError(error)
    }
}

impl From<ProjectDateTimeError> for TzError {
    fn from(error: ProjectDateTimeError) -> Self {
        Self::ProjectDateTimeError(error)
    }
}
