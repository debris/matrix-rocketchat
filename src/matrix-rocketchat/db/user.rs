use std::time::{SystemTime, UNIX_EPOCH};

use ruma_identifiers::{RoomId, UserId};

use api::MatrixApi;
use db::Room;
use errors::*;

/// A Matrix `User`.
#[derive(Debug)]
pub struct User {
    /// The users unique id on the Matrix server.
    pub matrix_user_id: UserId,
    /// The language the user prefers to get messages in.
    pub language: String,
    /// Time when the user sent the last message in seconds since UNIX_EPOCH
    pub last_message_sent: i64,
}

impl User {
    /// Checks if a users exists on the Matrix homeserver
    pub fn exists(matrix_api: &MatrixApi, matrix_user_id: &UserId) -> Result<bool> {
        return Ok(false);
    }

    /// Checks if a user is in a room.
    pub fn is_in_room(matrix_api: &MatrixApi, user_id: &UserId, matrix_room_id: RoomId) -> Result<bool> {
        let user_ids_in_room = Room::user_ids(matrix_api, matrix_room_id, None)?;
        Ok(user_ids_in_room.iter().any(|id| id == user_id))
    }
}
