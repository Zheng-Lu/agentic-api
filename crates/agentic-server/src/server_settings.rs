//! Gateway admission settings from the `[server]` table.
//!
//! Each setting resolves as CLI argument > environment variable >
//! configuration file > default. An explicit CLI argument short-circuits the
//! lower-priority sources, so a stale or malformed value inherited from the
//! environment cannot block startup when the operator names a valid value.

use std::env::VarError;
use std::num::NonZeroUsize;

use agentic_core::error::Error;
use agentic_server::app::DEFAULT_MAX_REQUEST_BODY_SIZE;

/// Environment override for the serialized request-size ceiling.
pub(crate) const MAX_REQUEST_BODY_SIZE_ENV: &str = "AGENTIC_MAX_REQUEST_BODY_SIZE_BYTES";

pub(crate) fn resolve_max_request_body_size(
    cli: Option<NonZeroUsize>,
    file: Option<NonZeroUsize>,
) -> Result<NonZeroUsize, Error> {
    resolve_max_request_body_size_value(cli, file, std::env::var(MAX_REQUEST_BODY_SIZE_ENV))
}

pub(crate) fn resolve_max_request_body_size_value(
    cli: Option<NonZeroUsize>,
    file: Option<NonZeroUsize>,
    value: Result<String, VarError>,
) -> Result<NonZeroUsize, Error> {
    if let Some(cli) = cli {
        return Ok(cli);
    }
    crate::parse_env_nonzero_usize_value(
        MAX_REQUEST_BODY_SIZE_ENV,
        value,
        file.unwrap_or(DEFAULT_MAX_REQUEST_BODY_SIZE),
    )
}
