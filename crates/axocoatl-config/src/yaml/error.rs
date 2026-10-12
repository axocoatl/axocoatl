//! The YAML error type: the message, and where in the input it happened.

use std::fmt::{self, Debug, Display};
use std::sync::Arc;

use serde::{de, ser};

/// An error reading or writing YAML.
///
/// Its `Display` names the line and column of the input it concerns, as
/// `... at line 3 column 7`, whenever the error has a position.
pub struct Error(Box<ErrorImpl>);

/// A position in the YAML input: line and column count from 1, the index
/// counts characters from 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Location {
    index: usize,
    line: usize,
    column: usize,
}

impl Location {
    /// Characters before the position.
    pub fn index(&self) -> usize {
        self.index
    }

    /// The line, from 1.
    pub fn line(&self) -> usize {
        self.line
    }

    /// The column, from 1.
    pub fn column(&self) -> usize {
        self.column
    }
}

/// Where an event starts in the input: line and column from 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Mark {
    pub(crate) index: usize,
    pub(crate) line: usize,
    pub(crate) column: usize,
}

impl Mark {
    pub(crate) fn from_parser(marker: &granit_parser::Marker) -> Mark {
        Mark {
            index: marker.index(),
            line: marker.line(),
            column: marker.col() + 1,
        }
    }
}

impl Display for Mark {
    fn fmt(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        write!(formatter, "line {} column {}", self.line, self.column)
    }
}

#[derive(Debug)]
pub(crate) struct Pos {
    mark: Mark,
    path: String,
}

#[derive(Debug)]
pub(crate) enum ErrorImpl {
    Message(String, Option<Pos>),
    Syntax(String, Mark),
    EndOfStream,
    MoreThanOneDocument(Option<Mark>),
    RecursionLimitExceeded(Mark),
    RepetitionLimitExceeded,
    BytesUnsupported,
    UnknownAnchor(Mark),
    SerializeNestedEnum,
    ComplexKey,
    Shared(Arc<ErrorImpl>),
}

pub(crate) fn new(inner: ErrorImpl) -> Error {
    Error(Box::new(inner))
}

pub(crate) fn shared(shared: Arc<ErrorImpl>) -> Error {
    Error(Box::new(ErrorImpl::Shared(shared)))
}

/// Give a message without a position the position of the event being read.
pub(crate) fn fix_mark(mut error: Error, mark: Mark, path: super::de::Path) -> Error {
    if let ErrorImpl::Message(_, none @ None) = error.0.as_mut() {
        *none = Some(Pos {
            mark,
            path: path.to_string(),
        });
    }
    error
}

impl Error {
    /// Where in the input the error happened, when it concerns a position.
    pub fn location(&self) -> Option<Location> {
        self.0.mark().map(|mark| Location {
            index: mark.index,
            line: mark.line,
            column: mark.column,
        })
    }
}

impl ErrorImpl {
    pub(crate) fn mark(&self) -> Option<Mark> {
        match self {
            ErrorImpl::Message(_, Some(Pos { mark, path: _ }))
            | ErrorImpl::Syntax(_, mark)
            | ErrorImpl::RecursionLimitExceeded(mark)
            | ErrorImpl::UnknownAnchor(mark)
            | ErrorImpl::MoreThanOneDocument(Some(mark)) => Some(*mark),
            ErrorImpl::Shared(err) => err.mark(),
            _ => None,
        }
    }

    fn message_no_mark(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ErrorImpl::Message(msg, None) => f.write_str(msg),
            ErrorImpl::Message(msg, Some(Pos { mark: _, path })) => {
                if path != "." {
                    write!(f, "{path}: ")?;
                }
                f.write_str(msg)
            }
            ErrorImpl::Syntax(msg, _) => f.write_str(msg),
            ErrorImpl::EndOfStream => f.write_str("EOF while parsing a value"),
            ErrorImpl::MoreThanOneDocument(_) => f.write_str(
                "deserializing from YAML containing more than one document is not supported",
            ),
            ErrorImpl::RecursionLimitExceeded(_) => f.write_str("recursion limit exceeded"),
            ErrorImpl::RepetitionLimitExceeded => f.write_str("repetition limit exceeded"),
            ErrorImpl::BytesUnsupported => {
                f.write_str("serialization and deserialization of bytes in YAML is not implemented")
            }
            ErrorImpl::UnknownAnchor(_) => f.write_str("unknown anchor"),
            ErrorImpl::SerializeNestedEnum => {
                f.write_str("serializing nested enums in YAML is not supported yet")
            }
            ErrorImpl::ComplexKey => {
                f.write_str("serializing a sequence or mapping as a YAML map key is not supported")
            }
            ErrorImpl::Shared(err) => err.message_no_mark(f),
        }
    }

    fn display(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.message_no_mark(f)?;
        if let Some(mark) = self.mark() {
            write!(f, " at {mark}")?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

impl Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.0.display(f)
    }
}

impl Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        struct MessageNoMark<'a>(&'a ErrorImpl);
        impl Display for MessageNoMark<'_> {
            fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
                self.0.message_no_mark(f)
            }
        }
        f.write_str("Error(")?;
        Debug::fmt(&MessageNoMark(&self.0).to_string(), f)?;
        if let Some(mark) = self.0.mark() {
            write!(f, ", line: {}, column: {}", mark.line, mark.column)?;
        }
        f.write_str(")")
    }
}

impl de::Error for Error {
    fn custom<T: Display>(msg: T) -> Self {
        new(ErrorImpl::Message(msg.to_string(), None))
    }
}

impl ser::Error for Error {
    fn custom<T: Display>(msg: T) -> Self {
        new(ErrorImpl::Message(msg.to_string(), None))
    }
}
