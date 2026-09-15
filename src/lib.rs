//! git-hydrate keeps large files in an object store and pointers in git, with
//! one copy on disk and no local cache.
//!
//! The pointer is git-lfs's. The filter is git's long-running filter process,
//! with a clean that hashes and a smudge that is the identity. The verbs move
//! bytes between the working tree and the remote on request. See the README
//! for the whole account.

pub mod cli;
pub mod commands;
pub mod config;
pub mod filter;
pub mod git;
pub mod hash;
pub mod pktline;
pub mod pointer;
pub mod progress;
pub mod remote;
pub mod tracked;
pub mod transfer;
