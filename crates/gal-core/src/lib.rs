//! Domain model and wire protocol shared by the Gal server and its clients.

pub mod model;
pub mod protocol;

pub use gal_ot::{self as ot, Delta};
pub use model::*;
pub use protocol::*;
