//! REST API types.

/// Matrix REST API
pub mod matrix;
/// Generic REST API
pub mod rest_api;
/// Rocket.Chat REST API
pub mod rocketchat;

pub use self::matrix::MatrixApi;
pub use self::rest_api::RestApi;
pub use self::rocketchat::RocketchatApi;
