//! Hinagata: Registry for classical ML models: versions, signatures,
//! lineage, stages, deployment descriptors and drift monitoring.

pub mod api;
pub mod artifacts;
pub mod attestation;
pub mod auth;
pub mod bundle;
pub mod config;
pub mod descriptor;
pub mod drift;
pub mod promotion;
pub mod quality;
pub mod registry;
pub mod signature;
pub mod signing;
