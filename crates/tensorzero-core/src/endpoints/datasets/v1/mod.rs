#[cfg(feature = "full-gateway")]
mod conversion_utils;
#[cfg(feature = "full-gateway")]
mod create_datapoints;
#[cfg(feature = "full-gateway")]
mod create_from_inferences;
#[cfg(feature = "full-gateway")]
mod delete_datapoints;
#[cfg(feature = "full-gateway")]
mod get_datapoints;
#[cfg(feature = "full-gateway")]
mod list_datasets;
#[cfg(feature = "full-gateway")]
mod update_datapoints;

pub mod types;

#[cfg(feature = "full-gateway")]
pub use create_datapoints::{create_datapoints, create_datapoints_handler};
#[cfg(feature = "full-gateway")]
pub use create_from_inferences::{create_from_inferences, create_from_inferences_handler};
#[cfg(feature = "full-gateway")]
pub use delete_datapoints::{
    delete_datapoints, delete_datapoints_handler, delete_dataset, delete_dataset_handler,
};
#[cfg(feature = "full-gateway")]
pub use get_datapoints::{
    get_datapoints, get_datapoints_by_dataset_handler, get_datapoints_handler, list_datapoints,
    list_datapoints_handler,
};
#[cfg(feature = "full-gateway")]
pub use list_datasets::{list_datasets, list_datasets_handler};
#[cfg(feature = "full-gateway")]
pub use update_datapoints::{
    update_datapoints, update_datapoints_handler, update_datapoints_metadata,
    update_datapoints_metadata_handler,
};
