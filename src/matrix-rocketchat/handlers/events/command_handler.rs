use diesel::Connection;
use diesel::sqlite::SqliteConnection;
use ruma_events::room::message::MessageEvent;
use ruma_events::room::message::MessageEventContent;
use ruma_identifiers::{RoomId, UserId};
use slog::Logger;

use MAX_ROCKETCHAT_SERVER_ID_LENGTH;
use api::{MatrixApi, RocketchatApi};
use api::rocketchat::Channel;
use config::Config;
use db::{NewRocketchatServer, NewUserOnRocketchatServer, RocketchatServer, Room, User, UserOnRocketchatServer};
use errors::*;
use handlers::rocketchat::{Credentials, Login};
use handlers::events::RoomHandler;
use i18n::*;

/// Handles command messages from the admin room
pub struct CommandHandler<'a> {
    config: &'a Config,
    connection: &'a SqliteConnection,
    logger: &'a Logger,
    matrix_api: &'a MatrixApi,
}

impl<'a> CommandHandler<'a> {
    /// Create a new `CommandHandler`.
    pub fn new(
        config: &'a Config,
        connection: &'a SqliteConnection,
        logger: &'a Logger,
        matrix_api: &'a MatrixApi,
    ) -> CommandHandler<'a> {
        CommandHandler {
            config: config,
            connection: connection,
            logger: logger,
            matrix_api: matrix_api,
        }
    }

    /// Handles command messages from an admin room
    pub fn process(&self, event: &MessageEvent, matrix_room_id: RoomId) -> Result<()> {
        let message = match event.content {
            MessageEventContent::Text(ref text_content) => text_content.body.clone(),
            _ => {
                debug!(self.logger, "Unknown event content type, skipping");
                return Ok(());
            }
        };

        if message.starts_with("connect") {
            debug!(self.logger, "Received connect command: {}", message);

            self.connect(event, &message)?;
        } else if message == "help" {
            debug!(self.logger, "Received help command");

            self.help(event)?;
        } else if message.starts_with("login") {
            debug!(self.logger, "Received login command");

            let rocketchat_server = self.get_rocketchat_server(matrix_room_id)?;
            self.login(event, &rocketchat_server, &message)?;
        } else if message == "list" {
            debug!(self.logger, "Received list command");

            let rocketchat_server = self.get_rocketchat_server(matrix_room_id)?;
            self.list_channels(event, &rocketchat_server)?;
        } else if message.starts_with("bridge") {
            debug!(self.logger, "Received bridge command");

            let rocketchat_server = self.get_rocketchat_server(matrix_room_id)?;
            self.bridge(event, &rocketchat_server, &message)?;
        } else if message.starts_with("unbridge") {
            debug!(self.logger, "Received unbridge command");

            let rocketchat_server = self.get_rocketchat_server(matrix_room_id)?;
            self.unbridge(event, &rocketchat_server, &message)?;
        } else {
            debug!(self.logger, "Skipping event, don't know how to handle command `{}`", message);
        }

        Ok(())
    }

    fn connect(&self, event: &MessageEvent, message: &str) -> Result<()> {
        self.connection
            .transaction(|| {
                if Room::is_connected(self.connection, self.matrix_api, event.room_id.clone())? {
                    bail_error!(
                        ErrorKind::RoomAlreadyConnected(event.room_id.to_string()),
                        t!(["errors", "room_already_connected"])
                    );
                }

                let mut command = message.split_whitespace().collect::<Vec<&str>>().into_iter();
                let rocketchat_url = command.by_ref().nth(1).unwrap_or_default();

                debug!(self.logger, "Connecting to Rocket.Chat server {}", rocketchat_url);

                let rocketchat_server = match command.by_ref().next() {
                    Some(token) => {
                        let rocketchat_id = command.by_ref().next().unwrap_or_default().to_string();
                        self.connect_new_rocktechat_server(
                            rocketchat_id,
                            rocketchat_url.to_string(),
                            token.to_string(),
                            &event.user_id,
                        )?
                    }
                    None => self.get_existing_rocketchat_server(rocketchat_url.to_string())?,
                };

                let new_user_on_rocketchat_server = NewUserOnRocketchatServer {
                    is_virtual_user: false,
                    matrix_user_id: event.user_id.clone(),
                    rocketchat_server_id: rocketchat_server.id,
                    rocketchat_user_id: None,
                    rocketchat_auth_token: None,
                    rocketchat_username: None,
                };

                UserOnRocketchatServer::upsert(self.connection, &new_user_on_rocketchat_server)?;
                self.matrix_api.set_room_topic(event.room_id.clone(), rocketchat_url.to_string())?;

                let body = CommandHandler::build_help_message(
                    self.connection,
                    self.matrix_api,
                    self.config.as_url.clone(),
                    event.room_id.clone(),
                    &event.user_id,
                )?;
                self.matrix_api.send_text_message_event(event.room_id.clone(), self.config.matrix_bot_user_id()?, body)?;

                Ok(info!(
                    self.logger,
                    "Successfully executed connect command for user {} and Rocket.Chat server {}",
                    event.user_id,
                    rocketchat_url
                ))
            })
            .map_err(Error::from)
    }

    fn connect_new_rocktechat_server(
        &self,
        rocketchat_server_id: String,
        rocketchat_url: String,
        token: String,
        matrix_user_id: &UserId,
    ) -> Result<RocketchatServer> {
        if rocketchat_server_id.is_empty() {
            bail_error!(ErrorKind::ConnectWithoutRocketchatServerId, t!(["errors", "connect_without_rocketchat_server_id"]));
        } else if rocketchat_server_id.len() > MAX_ROCKETCHAT_SERVER_ID_LENGTH ||
                   rocketchat_server_id.chars().any(|c| c.is_uppercase() || (!c.is_digit(36) && c != '_'))
        {
            bail_error!(
                ErrorKind::ConnectWithInvalidRocketchatServerId(rocketchat_server_id.clone()),
                t!(["errors", "connect_with_invalid_rocketchat_server_id"]).with_vars(vec![
                    ("rocketchat_server_id", rocketchat_server_id),
                    (
                        "max_rocketchat_server_id_length",
                        format!("{}", MAX_ROCKETCHAT_SERVER_ID_LENGTH)
                    ),
                ])
            );
        } else if RocketchatServer::find_by_id(self.connection, &rocketchat_server_id)?.is_some() {
            bail_error!(
                ErrorKind::RocketchatServerIdAlreadyInUse(rocketchat_server_id.clone()),
                t!(["errors", "rocketchat_server_id_already_in_use"]).with_vars(
                    vec![("rocketchat_server_id", rocketchat_server_id)],
                )
            );
        }

        if let Some(rocketchat_server) = RocketchatServer::find_by_url(self.connection, rocketchat_url.clone())? {
            if rocketchat_server.rocketchat_token.is_some() {
                bail_error!(
                    ErrorKind::RocketchatServerAlreadyConnected(rocketchat_url.clone()),
                    t!(["errors", "rocketchat_server_already_connected"]).with_vars(vec![
                        ("rocketchat_url", rocketchat_url),
                        ("matrix_user_id", matrix_user_id.to_string()),
                    ])
                );
            }
        }

        if RocketchatServer::find_by_token(self.connection, token.clone())?.is_some() {
            bail_error!(
                ErrorKind::RocketchatTokenAlreadyInUse(token.clone()),
                t!(["errors", "token_already_in_use"]).with_vars(vec![("token", token)])
            );
        }

        // see if we can reach the server and if the server has a supported API version
        RocketchatApi::new(rocketchat_url.clone(), self.logger.clone())?;

        let new_rocketchat_server = NewRocketchatServer {
            id: rocketchat_server_id,
            rocketchat_url: rocketchat_url,
            rocketchat_token: Some(token),
        };

        RocketchatServer::insert(self.connection, &new_rocketchat_server)
    }

    fn help(&self, event: &MessageEvent) -> Result<()> {
        let help_message = CommandHandler::build_help_message(
            self.connection,
            self.matrix_api,
            self.config.as_url.clone(),
            event.room_id.clone(),
            &event.user_id,
        )?;
        let bot_matrix_user_id = self.config.matrix_bot_user_id()?;
        self.matrix_api.send_text_message_event(event.room_id.clone(), bot_matrix_user_id, help_message)?;

        Ok(info!(self.logger, "Successfully executed help command for user {}", event.user_id))
    }

    fn login(&self, event: &MessageEvent, rocketchat_server: &RocketchatServer, message: &str) -> Result<()> {
        let mut command = message.split_whitespace().collect::<Vec<&str>>().into_iter();
        let username = command.by_ref().nth(1).unwrap_or_default();
        let password = command.by_ref().fold("".to_string(), |acc, x| acc + x);

        let credentials = Credentials {
            matrix_user_id: event.user_id.clone(),
            rocketchat_username: username.to_string(),
            password: password.to_string(),
            rocketchat_url: rocketchat_server.rocketchat_url.clone(),
        };
        let login = Login {
            config: self.config,
            connection: self.connection,
            logger: self.logger,
            matrix_api: self.matrix_api,
        };
        login.call(&credentials, rocketchat_server, Some(event.room_id.clone()))
    }

    fn list_channels(&self, event: &MessageEvent, rocketchat_server: &RocketchatServer) -> Result<()> {
        let user_on_rocketchat_server =
            UserOnRocketchatServer::find(self.connection, &event.user_id, rocketchat_server.id.clone())?;
        let rocketchat_api = RocketchatApi::new(rocketchat_server.rocketchat_url.clone(), self.logger.clone())?
            .with_credentials(
                user_on_rocketchat_server.rocketchat_user_id.unwrap_or_default(),
                user_on_rocketchat_server.rocketchat_auth_token.unwrap_or_default(),
            );
        let channels = rocketchat_api.channels_list()?;

        let bot_matrix_user_id = self.config.matrix_bot_user_id()?;
        let channels_list = self.build_channels_list(&rocketchat_server.id, &event.user_id, channels)?;
        let message = t!(["admin_room", "list_channels"]).with_vars(vec![("channel_list", channels_list)]);
        self.matrix_api.send_text_message_event(event.room_id.clone(), bot_matrix_user_id, message.l(DEFAULT_LANGUAGE))?;

        Ok(info!(self.logger, "Successfully listed channels for Rocket.Chat server {}", &rocketchat_server.rocketchat_url))
    }

    fn bridge(&self, event: &MessageEvent, rocketchat_server: &RocketchatServer, message: &str) -> Result<()> {
        let bot_matrix_user_id = self.config.matrix_bot_user_id()?;
        let user_on_rocketchat_server =
            UserOnRocketchatServer::find(self.connection, &event.user_id, rocketchat_server.id.clone())?;
        let rocketchat_api = RocketchatApi::new(rocketchat_server.rocketchat_url.clone(), self.logger.clone())?
            .with_credentials(
                user_on_rocketchat_server.rocketchat_user_id.clone().unwrap_or_default(),
                user_on_rocketchat_server.rocketchat_auth_token.clone().unwrap_or_default(),
            );

        let channels = rocketchat_api.channels_list()?;

        let mut command = message.split_whitespace().collect::<Vec<&str>>().into_iter();
        let channel_name = command.by_ref().nth(1).unwrap_or_default();

        let channel = match channels.iter().find(|channel| channel.name.clone().unwrap_or_default() == channel_name) {
            Some(channel) => channel,
            None => {
                bail_error!(
                    ErrorKind::RocketchatChannelNotFound(channel_name.to_string()),
                    t!(["errors", "rocketchat_channel_not_found"]).with_vars(vec![("channel_name", channel_name.to_string())])
                );
            }
        };

        let username = user_on_rocketchat_server.rocketchat_username.clone().unwrap_or_default();
        if !channel.usernames.iter().any(|u| u == &username) {
            bail_error!(
                ErrorKind::RocketchatJoinFirst(channel_name.to_string()),
                t!(["errors", "rocketchat_join_first"]).with_vars(vec![("channel_name", channel_name.to_string())])
            );
        }

        let room_handler = RoomHandler::new(self.config, self.connection, self.logger, self.matrix_api);
        let matrix_room_id = match Room::matrix_id_from_rocketchat_channel_id(
            self.config,
            self.matrix_api,
            &rocketchat_server.id,
            &channel.id,
        )? {
            Some(matrix_room_id) => {
                room_handler.bridge_existing_room(matrix_room_id.clone(), event.user_id.clone(), channel_name.to_string())?;
                matrix_room_id
            }
            None => {
                room_handler.bridge_new_room(
                    rocketchat_api,
                    rocketchat_server,
                    channel,
                    bot_matrix_user_id.clone(),
                    event.user_id.clone(),
                )?
            }
        };

        let matrix_room_alias_id = Room::build_room_alias_id(self.config, &rocketchat_server.id, &channel.id)?;
        self.matrix_api.put_canonical_room_alias(matrix_room_id.clone(), Some(matrix_room_alias_id))?;

        let message =
            t!(["admin_room", "room_successfully_bridged"]).with_vars(vec![
                (
                    "channel_name",
                    channel.name.clone().unwrap_or_else(|| channel.id.clone())
                ),
            ]);
        self.matrix_api.send_text_message_event(event.room_id.clone(), bot_matrix_user_id, message.l(DEFAULT_LANGUAGE))?;

        Ok(info!(self.logger, "Successfully bridged room {} to {}", &channel.id, &matrix_room_id))
    }

    fn unbridge(&self, event: &MessageEvent, rocketchat_server: &RocketchatServer, message: &str) -> Result<()> {
        let mut command = message.split_whitespace().collect::<Vec<&str>>().into_iter();
        let channel_name = command.nth(1).unwrap_or_default().to_string();

        let user_on_rocketchat_server =
            UserOnRocketchatServer::find(self.connection, &event.user_id, rocketchat_server.id.clone())?;
        let rocketchat_api = RocketchatApi::new(rocketchat_server.rocketchat_url.clone(), self.logger.clone())?
            .with_credentials(
                user_on_rocketchat_server.rocketchat_user_id.clone().unwrap_or_default(),
                user_on_rocketchat_server.rocketchat_auth_token.clone().unwrap_or_default(),
            );

        let matrix_room_id = match Room::matrix_id_from_rocketchat_channel_name(
            self.config,
            self.matrix_api,
            rocketchat_api.as_ref(),
            &rocketchat_server.id,
            channel_name.clone(),
        )? {
            Some(matrix_room_id) => matrix_room_id,
            None => {
                bail_error!(
                    ErrorKind::UnbridgeOfNotBridgedRoom(channel_name.to_string()),
                    t!(["errors", "unbridge_of_not_bridged_room"]).with_vars(vec![("channel_name", channel_name)])
                );
            }
        };

        let virtual_user_prefix = format!("@{}", self.config.sender_localpart);
        let user_ids: Vec<UserId> = Room::user_ids(self.matrix_api, matrix_room_id.clone(), None)?
            .into_iter()
            .filter(|id| !id.to_string().starts_with(&virtual_user_prefix))
            .collect();
        if !user_ids.is_empty() {
            let user_ids = user_ids.iter().map(|id| id.to_string()).collect::<Vec<String>>().join(", ");
            bail_error!(
                ErrorKind::RoomNotEmpty(channel_name.to_string(), user_ids.clone()),
                t!(["errors", "room_not_empty"]).with_vars(vec![("channel_name", channel_name), ("users", user_ids)])
            );
        }

        let rocketchat_channel_id = Room::rocketchat_channel_id(self.matrix_api, matrix_room_id.clone())?.unwrap_or_default();
        let room_alias_id = Room::build_room_alias_id(self.config, &rocketchat_server.id, &rocketchat_channel_id)?;
        self.matrix_api.put_canonical_room_alias(event.room_id.clone(), None)?;
        self.matrix_api.delete_room_alias(room_alias_id)?;

        let bot_matrix_user_id = self.config.matrix_bot_user_id()?;
        let message = t!(["admin_room", "room_successfully_unbridged"]).with_vars(vec![("channel_name", channel_name.clone())]);
        self.matrix_api.send_text_message_event(event.room_id.clone(), bot_matrix_user_id, message.l(DEFAULT_LANGUAGE))?;

        Ok(info!(self.logger, "Successfully unbridged room {}", channel_name.clone()))
    }

    fn get_existing_rocketchat_server(&self, rocketchat_url: String) -> Result<RocketchatServer> {
        let rocketchat_server: RocketchatServer = match RocketchatServer::find_by_url(self.connection, rocketchat_url)? {
            Some(rocketchat_server) => rocketchat_server,
            None => {
                bail_error!(ErrorKind::RocketchatTokenMissing, t!(["errors", "rocketchat_token_missing"]));
            }
        };

        Ok(rocketchat_server)
    }

    fn build_channels_list(
        &self,
        rocketchat_server_id: &str,
        matrix_user_id: &UserId,
        channels: Vec<Channel>,
    ) -> Result<String> {
        let user = UserOnRocketchatServer::find(self.connection, matrix_user_id, rocketchat_server_id.to_string())?;
        let mut channel_list = "".to_string();

        for channel in channels {
            let formatter = if Room::is_bridged_for_user(
                self.config,
                self.matrix_api,
                rocketchat_server_id,
                &channel.id,
                matrix_user_id,
            )?
            {
                "**"
            } else if channel.usernames.iter().any(|username| Some(username) == user.rocketchat_username.as_ref()) {
                "*"
            } else {
                ""
            };

            channel_list = channel_list + "*   " + formatter + &channel.name.unwrap_or(channel.id) + formatter + "\n\n";
        }

        Ok(channel_list)
    }

    fn get_rocketchat_server(&self, matrix_room_id: RoomId) -> Result<RocketchatServer> {
        match Room::rocketchat_server_for_admin_room(self.connection, self.matrix_api, matrix_room_id.clone())? {
            Some(rocketchat_server) => Ok(rocketchat_server),
            None => {
                Err(user_error!(ErrorKind::RoomNotConnected(matrix_room_id.to_string()), t!(["errors", "room_not_connected"])))
            }
        }
    }

    /// Build the help message depending on the status of the admin room (connected, user logged
    /// in, etc.).
    pub fn build_help_message(
        connection: &SqliteConnection,
        matrix_api: &MatrixApi,
        as_url: String,
        matrix_room_id: RoomId,
        matrix_user_id: &UserId,
    ) -> Result<String> {
        let message = match Room::rocketchat_server_for_admin_room(connection, matrix_api, matrix_room_id)? {
            Some(rocketchat_server) => {
                if UserOnRocketchatServer::find(connection, matrix_user_id, rocketchat_server.id)?.is_logged_in() {
                    t!(["admin_room", "usage_instructions"]).with_vars(
                        vec![("rocketchat_url", rocketchat_server.rocketchat_url)],
                    )
                } else {
                    t!(["admin_room", "login_instructions"]).with_vars(vec![
                        ("rocketchat_url", rocketchat_server.rocketchat_url),
                        ("as_url", as_url),
                        ("matrix_user_id", matrix_user_id.to_string()),
                    ])
                }
            }
            None => {
                let connected_servers = RocketchatServer::find_connected_servers(connection)?;
                let server_list = if connected_servers.is_empty() {
                    t!(["admin_room", "no_rocketchat_server_connected"]).l(DEFAULT_LANGUAGE)
                } else {
                    connected_servers.iter().fold("".to_string(), |init, rs| init + &format!("* {}\n", rs.rocketchat_url))
                };
                t!(["admin_room", "connection_instructions"]).with_vars(vec![("as_url", as_url), ("server_list", server_list)])
            }
        };

        Ok(message.l(DEFAULT_LANGUAGE))
    }
}
