//! Local-first, private cross-session memory for coding agents.
//!
//! Everything lives in a single SQLite file on the user's machine. Nothing in
//! this crate opens a network connection.

pub mod brief;
pub mod capture;
pub mod hook;
pub mod journal;
pub mod mcp;
pub mod paths;
pub mod project;
pub mod redact;
pub mod store;
mod text;
mod time;

use std::fmt;

#[derive(Debug)]
pub enum Error {
    Db(rusqlite::Error),
    Io(std::io::Error),
    Invalid(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Db(e) => write!(f, "database error: {e}"),
            Error::Io(e) => write!(f, "io error: {e}"),
            Error::Invalid(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for Error {}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Error::Db(e)
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
