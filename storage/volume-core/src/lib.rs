//! Shared native volume types and algorithms. No Kubernetes, CSI or VMM dependency.
mod model;
pub use model::*;
pub mod catalog;
#[cfg(feature = "content")]
pub mod content;
#[cfg(feature = "metadata")]
pub mod digest;
pub mod journal;
#[cfg(feature = "content")]
pub mod layers;
#[cfg(feature = "content")]
pub mod materialize;
pub mod object;
pub mod posix;

#[cfg(feature = "content")]
pub mod compact;
#[cfg(feature = "content")]
pub mod recontainerize;

#[cfg(feature = "content")]
pub mod object_layers;

#[cfg(feature = "content")]
pub mod engine;

#[cfg(feature = "content")]
pub mod repositories;

#[cfg(feature = "metadata")]
pub mod heads;

#[cfg(feature = "metadata")]
pub mod head_repository;

#[cfg(feature = "content")]
pub mod metrics;
#[cfg(feature = "content")]
pub mod oss_client;
