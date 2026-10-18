//! Crate-wide helpers that belong to no single layer.
//!
//! Both modules are used from several layers, which is why they cannot live
//! inside one of them: `config`, `sensors` and `app` all normalise names, and
//! both `sensors` and `actuator` run external commands.
//!
//! The rule for adding a file here: it must have a name that says what it does
//! (`process.rs`, not `misc.rs`). A module that only one layer uses belongs in
//! that layer instead.

pub mod name;
pub mod process;
