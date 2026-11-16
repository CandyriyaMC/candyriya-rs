//! Error types for configuration loading and validation.

use std::fmt;
use std::path::PathBuf;

/// A single validation problem, tied to a dotted field path.
///
/// Validation is best effort: as many problems as possible are collected
/// into one [`ConfigError::Validation`] instead of stopping at the first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidationProblem {
    /// Dotted path to the offending field, e.g. `advanced.compression-level`
    /// or `try`.
    pub field: String,
    /// What is wrong with the field, in plain English.
    pub message: String,
}

impl ValidationProblem {
    pub(crate) fn new(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            field: field.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ValidationProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.field, self.message)
    }
}

/// Errors produced by configuration loading.
///
/// All messages are written in plain English and are safe to show to the
/// operator. No variant ever carries forwarding-secret material.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The configuration file could not be read.
    #[error("failed to read configuration file {path}: {source}")]
    Read {
        /// Path that failed to load.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },

    /// The configuration file is not valid TOML. The message includes the
    /// line and column reported by the TOML parser.
    #[error("TOML syntax error in {path}: {message}")]
    Toml {
        /// Path that failed to parse.
        path: PathBuf,
        /// Full parser diagnostic, including `line` and `column`.
        message: String,
    },

    /// The TOML was syntactically valid but could not be decoded into the
    /// configuration schema (e.g. a string where an integer was expected).
    #[error("failed to decode configuration: {0}")]
    Decode(String),

    /// One or more semantic validation problems were found. Every problem
    /// that could be detected is included.
    #[error("invalid configuration:\n{0}")]
    Validation(ValidationProblems),

    /// The forwarding secret file could not be read, decoded, created or
    /// protected. The secret itself is never included in the message.
    #[error("forwarding secret: {0}")]
    Secret(String),
}

impl ConfigError {
    pub(crate) fn validation(problems: Vec<ValidationProblem>) -> Self {
        Self::Validation(ValidationProblems { problems })
    }

    /// The collected validation problems, if this is a
    /// [`ConfigError::Validation`], otherwise an empty slice.
    pub fn problems(&self) -> &[ValidationProblem] {
        match self {
            Self::Validation(problems) => &problems.problems,
            _ => &[],
        }
    }
}

/// Newtype wrapping the collected validation problems so `Display` can
/// render one line per problem.
#[derive(Debug)]
pub struct ValidationProblems {
    problems: Vec<ValidationProblem>,
}

impl ValidationProblems {
    /// The problems that were collected.
    pub fn as_slice(&self) -> &[ValidationProblem] {
        &self.problems
    }
}

impl std::ops::Deref for ValidationProblems {
    type Target = [ValidationProblem];

    fn deref(&self) -> &Self::Target {
        &self.problems
    }
}

impl fmt::Display for ValidationProblems {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, problem) in self.problems.iter().enumerate() {
            if index > 0 {
                writeln!(f)?;
            }
            write!(f, "- {problem}")?;
        }
        Ok(())
    }
}
