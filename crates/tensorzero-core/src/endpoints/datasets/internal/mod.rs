#[cfg(feature = "full-gateway")]
mod clone_datapoints;
#[cfg(feature = "full-gateway")]
mod get_datapoint_count;

#[cfg(feature = "full-gateway")]
pub use clone_datapoints::{CloneDatapointsResponse, clone_datapoints_handler};
#[cfg(feature = "full-gateway")]
pub use get_datapoint_count::{GetDatapointCountResponse, get_datapoint_count_handler};
