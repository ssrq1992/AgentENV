//! Custom extension integration: an optional external HTTP service
//! (`[custom_extension].url`) extending sandbox behavior through lifecycle
//! hooks. Generic external storage is independently configured in sandbox::storage.

pub(crate) mod client;

pub(crate) use client::{
    custom_extension_params_is_empty, CustomExtensionClient, CustomExtensionHookGuard,
    CustomExtensionParams,
};
