use reqwest::Url;
use std::fmt;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CrawlError<T: fmt::Debug> {
    #[error("Received response with non 2xx status {:?} for {:?} carrying state: {:?}", .response, .request_url, .state)]
    NoSuccessResponse {
        request_url: Option<Url>,
        /// The received response
        response: reqwest::Response,
        /// The attached state to this request, if any
        state: Option<T>,
    },
    #[error("Failed to construct a request: {} while carrying state: {:?}", .error, .state)]
    FailedToBuildRequest {
        error: reqwest::Error,
        state: Option<T>,
        depth: usize,
    },
    #[error("Failed to process invalid request while carrying state: {:?}", .state)]
    InvalidRequest {
        request: reqwest::Request,
        state: Option<T>,
    },
    #[error("Reached max depth at {} while carrying state: {:?}", .depth ,.state)]
    ReachedMaxDepth {
        request: reqwest::RequestBuilder,
        state: Option<T>,
        depth: usize,
    },
    #[error("Failed to fetch robots.txt from {}", .host)]
    RobotsTxtError { host: String },
    #[error("Rejected a request, because its url is disallowed due to {}, while carrying state: {:?}", .reason, .state)]
    DisallowedRequest {
        reason: DisallowReason,
        request: reqwest::Request,
        state: Option<T>,
    },
}

impl<T: fmt::Debug> CrawlError<T> {
    /// Get the state this error is carrying
    pub fn state(&self) -> Option<&T> {
        match self {
            CrawlError::NoSuccessResponse { state, .. } => state.as_ref(),
            CrawlError::FailedToBuildRequest { state, .. } => state.as_ref(),
            CrawlError::InvalidRequest { state, .. } => state.as_ref(),
            CrawlError::ReachedMaxDepth { state, .. } => state.as_ref(),
            CrawlError::RobotsTxtError { .. } => None,
            CrawlError::DisallowedRequest { state, .. } => state.as_ref(),
        }
    }

    /// Recover the state this error may carrying
    pub fn into_state(self) -> Option<T> {
        match self {
            CrawlError::NoSuccessResponse { state, .. } => state,
            CrawlError::FailedToBuildRequest { state, .. } => state,
            CrawlError::InvalidRequest { state, .. } => state,
            CrawlError::ReachedMaxDepth { state, .. } => state,
            CrawlError::RobotsTxtError { .. } => None,
            CrawlError::DisallowedRequest { state, .. } => state,
        }
    }
}

#[derive(Debug, Clone)]
pub enum DisallowReason {
    /// Request disallowed by respecting robots.txt
    RobotsTxt,
    /// Request disallowed by user config
    User,
}

impl fmt::Display for DisallowReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DisallowReason::RobotsTxt => {
                write!(f, "URL blocked by robots.txt")
            }
            DisallowReason::User => {
                write!(f, "URL blocked by user config")
            }
        }
    }
}

#[derive(Debug, Copy, Clone, Error)]
#[error("Received response with unexpected status {0}")]
pub struct UnexpectedStatusError(u16);

impl UnexpectedStatusError {
    pub fn new(status: u16) -> Self {
        UnexpectedStatusError(status)
    }

    pub fn status(&self) -> u16 {
        self.0
    }
}
