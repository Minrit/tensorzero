#[cfg(feature = "full-gateway")]
mod get_inferences;

pub mod types;

#[cfg(feature = "full-gateway")]
pub use get_inferences::{
    get_inferences, get_inferences_handler, list_inferences, list_inferences_handler,
};
