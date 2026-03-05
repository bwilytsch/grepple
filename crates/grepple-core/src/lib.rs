pub mod app;
pub mod error;
pub mod installer;
pub mod log_ops;
pub mod mcp;
pub mod model;
pub mod runtime;
pub mod storage;

pub use app::{Grepple, GreppleConfig};
pub use error::{GreppleError, Result};
