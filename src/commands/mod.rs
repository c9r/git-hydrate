//! The verbs, one module each.

pub mod adopt;
pub mod drop;
pub mod env;
pub mod export;
pub mod install;
pub mod precommit;
pub mod prepush;
pub mod pull;
pub mod push;
pub mod status;
pub mod track;
pub mod verify;

use std::collections::HashMap;

use anyhow::Result;
use futures::{StreamExt, stream};

use crate::remote::Remote;

/// Remote lookups in flight at once.
const LOOKUPS_IN_FLIGHT: usize = 16;

/// Asks the remote for the size of each object, many at a time.
pub async fn head_all(remote: &Remote, oids: &[String]) -> Result<HashMap<String, Option<u64>>> {
    let results: Vec<Result<(String, Option<u64>)>> = stream::iter(oids.iter().cloned())
        .map(|oid| async move {
            let size = remote.head(&oid).await?;
            Ok((oid, size))
        })
        .buffer_unordered(LOOKUPS_IN_FLIGHT)
        .collect()
        .await;
    results.into_iter().collect()
}
