//! Native capture and encoded frames, independent of the application interface.
pub mod capture;
mod dave;
pub mod transport;

pub use dave::Identity;
