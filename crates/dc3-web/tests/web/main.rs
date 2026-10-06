//! Integration tests of the web application through its router, with an
//! in-memory backend and a real search index, plus one end-to-end test
//! against PostgreSQL when `DATABASE_URL` is set.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod api;
mod common;
mod pages;
mod search;
mod security;
mod serve;
mod store_db;
mod torrent;
