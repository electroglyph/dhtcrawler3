//! `sqlx::migrate!` embeds whatever is in `migrations/` at compile time, but
//! Cargo only tracks the files the macro knew about at the last compile: a
//! newly added migration file alone never dirties this crate, so a release
//! build can ship a stale embedded list and `migrate` then exits 0 having
//! applied nothing. Watching the directory forces a rebuild on any change
//! (add, modify, delete).
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
