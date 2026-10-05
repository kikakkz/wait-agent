//! The WebUI service (issue #131). Slice 1 is an axum skeleton (`GET
//! `/healthz` only) whose process enrolls itself into the pinned local relay
//! as a regular node: no in-process special casing, no dashboard data plane,
//! no authentication (both land in later slices).

pub mod serve;

#[cfg(test)]
mod tests;
