//! Concrete providers and the built-in endpoint presets.

#[cfg(feature = "bedrock")]
pub mod bedrock;
mod cloud_code;
mod discovery;
mod generated;
pub mod http;
mod logins;
pub mod presets;
pub mod resets;
mod table;
pub mod usage;
#[cfg(feature = "vertex")]
pub mod vertex;
