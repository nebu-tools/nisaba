use crate::protocol::close;

/// Why a frame could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("frame ends early")]
    Truncated,
    #[error("unknown frame type {0}")]
    UnknownType(u8),
    #[error("blob of {0} bytes is larger than allowed")]
    TooLarge(usize),
    #[error("{0} bytes after the end of the frame")]
    Trailing(usize),
    #[error("expected part {expected}, got {got}")]
    PartNumber { expected: u32, got: u32 },
    #[error("parts out of order: {0}")]
    PartsOrder(&'static str),
}

/// Why the room refused a frame or a client.
#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error("the server speaks protocol {expected}; reload the page")]
    Version { expected: u8 },
    #[error("this frame is not allowed here")]
    Unexpected,
    #[error("this role may not change the project")]
    Forbidden,
    #[error("{0} bytes are more than allowed")]
    TooLarge(usize),
    /// The project's stored size limit (`Limits::max_document`).
    #[error("the project is at its size limit: {need} bytes do not fit in the {left} left")]
    ProjectFull { need: usize, left: usize },
    #[error("the project has too many connections")]
    Full,
    #[error("the client did not read its frames in time")]
    TooSlow,
    #[error("the update did not arrive in time; it will be sent again")]
    TimedOut,
    /// Unfinished large updates hold the space this one needs, in the
    /// server's budget or in the project's quota. It ends when they do.
    #[error("the server is busy with other large updates; try again later")]
    Busy,
    /// Access to the project ended before the update was written.
    #[error("access to the project ended")]
    Ended,
    #[error("not a Loro update: {0}")]
    Loro(String),
    #[error("storage: {0}")]
    Storage(String),
}

impl SyncError {
    /// The WebSocket close code the session sends for this error.
    #[must_use]
    pub fn close_code(&self) -> u16 {
        match self {
            Self::Frame(FrameError::TooLarge(_)) | Self::TooLarge(_) | Self::ProjectFull { .. } => {
                close::TOO_LARGE
            }
            Self::Frame(_) | Self::Unexpected | Self::Loro(_) => close::MALFORMED,
            Self::Version { .. } => close::VERSION,
            Self::Forbidden => close::FORBIDDEN,
            Self::Full => close::FULL,
            Self::TooSlow => close::TOO_SLOW,
            Self::TimedOut => close::TIMED_OUT,
            Self::Busy => close::TRY_AGAIN,
            Self::Ended => close::SIGNED_OUT,
            Self::Storage(_) => close::SERVER_ERROR,
        }
    }
}

impl From<loro::LoroError> for SyncError {
    fn from(e: loro::LoroError) -> Self {
        Self::Loro(e.to_string())
    }
}

impl From<loro::LoroEncodeError> for SyncError {
    fn from(e: loro::LoroEncodeError) -> Self {
        Self::Loro(e.to_string())
    }
}
