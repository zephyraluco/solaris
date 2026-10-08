//! The wire protocols, one module each.
//!
//! Each module is a pure function of its inputs — build the request body, then
//! interpret the streamed events — so every protocol can be tested from a
//! recorded fixture. The transport lives in [`crate::http`], the retry loop in
//! [`crate::provider`], and [`crate::wire`] decides which of these to call.

pub(crate) mod anthropic;
pub(crate) mod openai;
pub(crate) mod responses;
