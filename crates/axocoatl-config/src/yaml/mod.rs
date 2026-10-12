//! YAML for Axocoatl: configurations, loadouts, egress routes and workflow
//! graphs.
//!
//! [granit-parser](https://crates.io/crates/granit-parser), a YAML 1.2
//! parser that forbids unsafe code and limits nesting to 255 block and 255
//! flow collections, turns the text into events. This module turns the
//! events into Rust values with serde the way serde_yaml 0.9 did, which
//! Axocoatl used until 1.3.1:
//!
//! - A plain scalar read where any type fits (`serde_json::Value`, an
//!   untagged or internally tagged enum) is null for `~`, `null`, `Null`,
//!   `NULL` and an empty value; a boolean only for `true`, `True`, `TRUE`,
//!   `false`, `False` and `FALSE` (not `yes`, `no`, `on`, `off`); an integer
//!   for decimal, `0x`, `0o` and `0b` numbers, but not for digits with a
//!   leading zero such as `017`, which stay a string, nor with underscores;
//!   a float for YAML 1.2 floats, `.inf` and `.nan`; else a string.
//! - A scalar read into a string is its text, whatever it looks like:
//!   `key: 017`, `key: true` and `key: ~` read into a `String` as `"017"`,
//!   `"true"` and `"~"`, and an empty `key:` as `""`.
//! - A boolean, integer or float field takes only a plain scalar of that
//!   type: `"5"` in quotes is not a number, `yes` is not a boolean.
//! - `<<` is an ordinary key: merge keys are not expanded.
//! - Anchors and aliases are followed, with serde_yaml's limit on how often
//!   (100 times the number of events in the document) against alias bombs,
//!   and nesting is limited to 128 levels.
//! - A duplicate key is refused by a struct and by [`Value`]; a map type
//!   such as `HashMap` keeps the last value, as it did.
//! - An enum variant with data is written with a local tag, `!Variant value`;
//!   a unit variant is its name.
//! - More than one document in a stream is refused.
//!
//! Where libyaml (under serde_yaml) and granit-parser disagree, this module
//! keeps libyaml's reading: an empty value is `""` rather than `~`, a block
//! scalar that ends the input without a line break has none, and a quoted
//! string's continuation lines may be indented less than YAML 1.2 asks. The
//! few documents that still read differently, all of which libyaml refused
//! or nested deeper than 255 collections, are listed in
//! `tests/yaml_differential.rs`, which checks every YAML document of the
//! repository against serde_yaml's recorded results.
//!
//! Errors name the line and column of the input (`at line 3 column 7`) and,
//! for a value of the wrong type, its path (`agents[0].model: ...`).

mod de;
mod error;
mod load;
mod ser;
mod value;

pub use de::from_str;
pub use error::{Error, Location};
pub use ser::to_string;
pub use value::{Number, Value};

#[cfg(test)]
mod tests;
