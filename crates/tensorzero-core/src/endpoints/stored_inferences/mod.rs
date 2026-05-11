#[cfg(feature = "full-gateway")]
mod render_samples;
pub mod v1;

#[cfg(feature = "full-gateway")]
pub use render_samples::render_samples;
