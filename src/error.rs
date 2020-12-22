use thiserror::Error;
pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Reqwest(#[from] reqwest::Error),
}
