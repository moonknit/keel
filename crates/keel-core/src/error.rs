use thiserror::Error;

#[derive(Debug, Error)]
pub enum KeelError {
    #[error("{0}")]
    Msg(String),
}

pub type Result<T> = std::result::Result<T, KeelError>;
