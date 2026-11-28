//! Native personal-network client. Network and durable writes run off the render thread.
mod api;
pub mod connection;
mod identity;
pub mod outbox;
pub use api::*;
pub use connection::{Connection, Endpoint, Refused};
pub use identity::{
    ClientKey, Directory, Enrollment, Entry, Progress, Session, create_network, recover,
};
pub use vibeface_protocol::workspace as wire;
