use std::collections::HashMap;
use std::convert::TryFrom;

use diesel::sqlite::SqliteConnection;
use diesel::Connection;
use iron::url::Host;
use ruma_events::room::member::{MemberEvent, MembershipState};
use ruma_identifiers::{RoomId, UserId};
use slog::Logger;

use api::{MatrixApi, RocketchatApi};
use api::rocketchat::Channel;
use config::Config;
use db::{RocketchatServer, Room, User};
use errors::*;
use handlers::ErrorNotifier;
use handlers::rocketchat::VirtualUserHandler;
use i18n::*;
use log;
use serde_json::{self, Value};
use super::CommandHandler;

/// Handles room events
pub struct RoomHandler<'a> {
    config: &'a Config,
    connection: &'a SqliteConnection,
    logger: &'a Logger,
    matrix_api: &'a MatrixApi,
}

impl<'a> RoomHandler<'a> {
    /// Create a new `RoomHandler`.
    pub fn new(
        config: &'a Config,
        connection: &'a SqliteConnection,
        logger: &'a Logger,
        matrix_api: &'a MatrixApi,
    ) -> RoomHandler<'a> {
        RoomHandler {
            config: config,
            connection: connection,
            logger: logger,
            matrix_api: matrix_api,
        }

    }

    /// Handles room membership changes
    pub fn process(&self, event: &MemberEvent) -> Result<()> {
        let matrix_bot_user_id = self.config.matrix_bot_user_id()?;
        let state_key = UserId::try_from(&event.state_key).chain_err(|| ErrorKind::InvalidUserId(event.state_key.clone()))?;
        let addressed_to_matrix_bot = state_key == matrix_bot_user_id;

        match event.content.membership {
            MembershipState::Invite if addressed_to_matrix_bot => {
                debug!(self.logger, "Bot `{}` got invite for room `{}`", matrix_bot_user_id, event.room_id);

                self.handle_bot_invite(event.room_id.clone(), matrix_bot_user_id)?;
            }
            MembershipState::Join if addressed_to_matrix_bot => {
                debug!(self.logger, "Received join event for bot user {} and room {}", matrix_bot_user_id, event.room_id);

                let unsigned: HashMap<String, Value> = serde_json::from_value(event.unsigned.clone().unwrap_or_default())
                    .unwrap_or_default();
                let inviter_id = match unsigned.get("prev_sender") {
                    Some(prev_sender) => {
                        let raw_id: String = serde_json::from_value(prev_sender.clone()).unwrap_or_else(|_| "".to_string());
                        let inviter_id = UserId::try_from(&raw_id).chain_err(|| ErrorKind::InvalidUserId(raw_id))?;
                        Some(inviter_id)
                    }
                    None => None,
                };

                self.handle_bot_join(event.room_id.clone(), matrix_bot_user_id, inviter_id)?;
            }
            MembershipState::Join => {
                debug!(self.logger, "Received join event for user {} and room {}", &state_key, &event.room_id);

                self.handle_user_join(event.room_id.clone())?;
            }
            MembershipState::Leave if !addressed_to_matrix_bot => {
                debug!(self.logger, "User {} left room {}", event.user_id, event.room_id);

                self.handle_user_leave(event.room_id.clone())?;
            }
            _ => {
                let msg = format!(
                    "Skipping event, don't know how to handle membership state `{}` with state key `{}`",
                    event.content.membership,
                    event.state_key
                );
                debug!(self.logger, "{}", msg);
            }
        }

        Ok(())
    }

    /// Bridges a new room between Rocket.Chat and Matrix. It creates the room on the Matrix
    /// homeserver and manages the rooms virtual users.
    pub fn bridge_new_room(
        &self,
        rocketchat_api: Box<RocketchatApi>,
        rocketchat_server: &RocketchatServer,
        channel: &Channel,
        room_creator_id: UserId,
        invited_user_id: UserId,
    ) -> Result<RoomId> {
        debug!(self.logger, "Briding new room, Rocket.Chat channel: {}", channel.name.clone().unwrap_or_default());
        let matrix_room_id = self.create_room(
            channel.id.clone(),
            rocketchat_server.id.clone(),
            room_creator_id,
            invited_user_id,
            channel.name.clone(),
        )?;
        let matrix_room_alias_id = Room::build_room_alias_id(self.config, &rocketchat_server.id, &channel.id)?;
        self.matrix_api.put_canonical_room_alias(matrix_room_id.clone(), Some(matrix_room_alias_id))?;
        self.add_virtual_users_to_room(rocketchat_api, channel, rocketchat_server.id.clone(), matrix_room_id.clone())?;
        Ok(matrix_room_id)
    }

    /// Bridges a room that is already bridged (for other users) for a new user.
    pub fn bridge_existing_room(
        &self,
        matrix_room_id: RoomId,
        matrix_user_id: UserId,
        rocketchat_channel_name: String,
    ) -> Result<()> {
        debug!(self.logger, "Briding existing room, Rocket.Chat channel: {}", rocketchat_channel_name);
        if User::is_in_room(self.matrix_api, &matrix_user_id, matrix_room_id.clone())? {
            bail_error!(
                ErrorKind::RocketchatChannelAlreadyBridged(rocketchat_channel_name.clone()),
                t!(["errors", "rocketchat_channel_already_bridged"]).with_vars(vec![("channel_name", rocketchat_channel_name)])
            );
        }

        let bot_matrix_user_id = self.config.matrix_bot_user_id()?;
        self.matrix_api.invite(matrix_room_id, matrix_user_id, bot_matrix_user_id)
    }

    fn handle_bot_invite(&self, matrix_room_id: RoomId, invited_user_id: UserId) -> Result<()> {
        if !self.config.accept_remote_invites && self.is_remote_invite(&matrix_room_id)? {
            info!(
                self.logger,
                "Bot was invited by a user from another homeserver ({}). \
                  Ignoring the invite because remote invites are disabled.",
                &matrix_room_id
            );
            return Ok(());
        }

        self.matrix_api.join(matrix_room_id, invited_user_id)?;

        Ok(())
    }

    fn handle_bot_join(&self, matrix_room_id: RoomId, matrix_bot_user_id: UserId, inviter_id: Option<UserId>) -> Result<()> {
        let is_admin_room = match Room::is_admin_room(self.matrix_api, self.config, matrix_room_id.clone()) {
            Ok(is_admin_room) => is_admin_room,
            Err(err) => {
                warn!(
                    self.logger,
                    "Could not determine if the room that the bot user was invited to is an admin room or not, bot is leaving"
                );
                self.handle_admin_room_setup_error(&err, matrix_room_id, matrix_bot_user_id);
                return Err(err);
            }
        };

        if is_admin_room {
            self.setup_admin_room(matrix_room_id.clone(), matrix_bot_user_id.clone(), inviter_id)?;
        }

        // leave direct message room, the bot only joined it to be able to read the room members
        if Room::is_direct_message_room(self.matrix_api, matrix_room_id.clone())? {
            self.matrix_api.leave_room(matrix_room_id.clone(), matrix_bot_user_id)?;
        }

        Ok(())
    }

    fn setup_admin_room(&self, matrix_room_id: RoomId, matrix_bot_user_id: UserId, inviter_id: Option<UserId>) -> Result<()> {
        debug!(self.logger, "Setting up a new admin room with id {}", matrix_room_id);

        let inviter_id = match inviter_id {
            Some(inviter_id) => inviter_id,
            None => {
                bail_error!(ErrorKind::InviterUnknown(matrix_room_id.clone()), t!(["errors", "inviter_unknown"]));
            }
        };

        if let Err(err) = self.is_admin_room_valid(matrix_room_id.clone(), &inviter_id) {
            info!(self.logger, "Admin room {} is not valid, bot will leave and forget the room", matrix_room_id);
            self.handle_admin_room_setup_error(&err, matrix_room_id, matrix_bot_user_id);
            return Ok(());
        }

        self.connection.transaction(|| {
            match CommandHandler::build_help_message(
                self.connection,
                self.matrix_api,
                self.config.as_url.clone(),
                matrix_room_id.clone(),
                &inviter_id,
            ) {
                Ok(body) => {
                    self.matrix_api.send_text_message_event(matrix_room_id.clone(), matrix_bot_user_id, body)?;
                }
                Err(err) => {
                    log::log_info(self.logger, &err);
                }
            }

            let room_name = t!(["defaults", "admin_room_display_name"]).l(DEFAULT_LANGUAGE);
            if let Err(err) = self.matrix_api.set_room_name(matrix_room_id, room_name) {
                log::log_info(self.logger, &err);
            }

            Ok(())
        })
    }

    fn handle_user_join(&self, matrix_room_id: RoomId) -> Result<()> {
        if Room::is_admin_room(self.matrix_api, self.config, matrix_room_id.clone())? &&
            !self.is_private_room(matrix_room_id.clone())?
        {
            info!(self.logger, "Another user join the admin room {}, bot user is leaving", matrix_room_id);
            let bot_matrix_user_id = self.config.matrix_bot_user_id()?;
            let body = t!(["errors", "other_user_joined"]).l(DEFAULT_LANGUAGE);
            self.matrix_api.send_text_message_event(matrix_room_id.clone(), bot_matrix_user_id.clone(), body)?;
            self.leave_and_forget_room(matrix_room_id, bot_matrix_user_id)?;
        }
        Ok(())
    }

    fn handle_user_leave(&self, matrix_room_id: RoomId) -> Result<()> {
        if Room::is_admin_room(self.matrix_api, self.config, matrix_room_id.clone())? {
            let bot_matrix_user_id = self.config.matrix_bot_user_id()?;
            return self.leave_and_forget_room(matrix_room_id, bot_matrix_user_id);
        }

        Ok(())
    }

    fn leave_and_forget_room(&self, matrix_room_id: RoomId, matrix_user_id: UserId) -> Result<()> {
        self.matrix_api.leave_room(matrix_room_id.clone(), matrix_user_id)?;
        self.matrix_api.forget_room(matrix_room_id)
    }

    fn is_remote_invite(&self, matrix_room_id: &RoomId) -> Result<bool> {
        let hs_hostname =
            Host::parse(&self.config.hs_domain).chain_err(|| ErrorKind::InvalidHostname(self.config.hs_domain.clone()))?;
        Ok(matrix_room_id.hostname().ne(&hs_hostname))
    }

    fn is_admin_room_valid(&self, matrix_room_id: RoomId, inviter_id: &UserId) -> Result<()> {
        debug!(self.logger, "Validating admin room");
        let room_creator_id = self.matrix_api.get_room_creator(matrix_room_id.clone())?;
        if inviter_id != &room_creator_id {
            let err = ErrorKind::OnlyRoomCreatorCanInviteBotUser(inviter_id.clone(), matrix_room_id.clone(), room_creator_id);
            bail_error!(err, t!(["errors", "only_room_creator_can_invite_bot_user"]));
        }

        if !self.is_private_room(matrix_room_id.clone())? {
            bail_error!(ErrorKind::TooManyUsersInAdminRoom(matrix_room_id), t!(["errors", "too_many_members_in_room"]));
        }

        Ok(())
    }

    fn is_private_room(&self, matrix_room_id: RoomId) -> Result<bool> {
        Ok(Room::user_ids(self.matrix_api, matrix_room_id, None)?.len() <= 2)
    }

    /// Create a room on the Matrix homeserver with the power levels for a bridged room.
    pub fn create_room(
        &self,
        rocketchat_channel_id: String,
        rocketchat_server_id: String,
        room_creator_id: UserId,
        invited_user_id: UserId,
        room_display_name: Option<String>,
    ) -> Result<RoomId> {
        let matrix_room_alias_id = Room::build_room_alias_name(self.config, &rocketchat_server_id, &rocketchat_channel_id);
        let matrix_room_id =
            self.matrix_api.create_room(room_display_name.clone(), Some(matrix_room_alias_id), &room_creator_id)?;
        debug!(self.logger, "Successfully created room, matrix_room_id is {}", &matrix_room_id);
        self.matrix_api.set_default_powerlevels(matrix_room_id.clone(), room_creator_id.clone())?;
        debug!(self.logger, "Successfully set powerlevels for room {}", &matrix_room_id);
        self.matrix_api.invite(matrix_room_id.clone(), invited_user_id.clone(), room_creator_id.clone())?;
        debug!(self.logger, "{} successfully invited {} into room {}", &room_creator_id, &invited_user_id, &matrix_room_id);

        Ok(matrix_room_id)
    }

    /// Add all users that are in a Rocket.Chat room to the Matrix room.
    pub fn add_virtual_users_to_room(
        &self,
        rocketchat_api: Box<RocketchatApi>,
        channel: &Channel,
        rocketchat_server_id: String,
        matrix_room_id: RoomId,
    ) -> Result<()> {
        debug!(self.logger, "Starting to add virtual users to room {}", matrix_room_id);

        let virtual_user_handler = VirtualUserHandler {
            config: self.config,
            connection: self.connection,
            logger: self.logger,
            matrix_api: self.matrix_api,
        };

        //TODO: Check if a max number of users per channel has to be defined to avoid problems when
        //there are several thousand users in a channel.
        let bot_matrix_user_id = self.config.matrix_bot_user_id()?;
        for username in &channel.usernames {
            let rocketchat_user = rocketchat_api.users_info(username)?;
            let user_on_rocketchat_server =
                virtual_user_handler.find_or_register(rocketchat_server_id.clone(), rocketchat_user.id, username.to_string())?;
            virtual_user_handler.add_to_room(
                user_on_rocketchat_server.matrix_user_id,
                bot_matrix_user_id.clone(),
                matrix_room_id.clone(),
            )?;
        }

        debug!(self.logger, "Successfully added {} virtual users to room {}", channel.usernames.len(), matrix_room_id);

        Ok(())
    }

    fn handle_admin_room_setup_error(&self, err: &Error, matrix_room_id: RoomId, matrix_bot_user_id: UserId) {
        let error_notifier = ErrorNotifier {
            config: self.config,
            connection: self.connection,
            logger: self.logger,
            matrix_api: self.matrix_api,
        };
        if let Err(err) = error_notifier.send_message_to_user(err, matrix_room_id.clone(), &matrix_bot_user_id) {
            log::log_error(self.logger, &err);
        }

        if let Err(err) = self.leave_and_forget_room(matrix_room_id.clone(), matrix_bot_user_id.clone()) {
            log::log_error(self.logger, &err);
        }
    }
}
