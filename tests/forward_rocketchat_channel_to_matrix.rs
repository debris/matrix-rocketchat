#![feature(try_from)]

extern crate iron;
extern crate matrix_rocketchat;
extern crate matrix_rocketchat_test;
extern crate reqwest;
extern crate router;
extern crate ruma_client_api;
extern crate ruma_identifiers;
extern crate serde_json;

use std::collections::HashMap;
use std::convert::TryFrom;

use iron::{Chain, status};
use matrix_rocketchat::api::{MatrixApi, RestApi};
use matrix_rocketchat::api::rocketchat::Message;
use matrix_rocketchat::db::{Room, User, UserOnRocketchatServer};
use matrix_rocketchat_test::{DEFAULT_LOGGER, MessageForwarder, RS_TOKEN, Test, default_timeout, handlers, helpers};
use reqwest::{Method, StatusCode};
use ruma_client_api::Endpoint;
use ruma_client_api::r0::membership::invite_user::Endpoint as InviteUserEndpoint;
use ruma_client_api::r0::membership::join_room_by_id::Endpoint as JoinRoomByIdEndpoint;
use ruma_client_api::r0::profile::set_display_name::Endpoint as SetDisplayNameEndpoint;
use ruma_client_api::r0::send::send_message_event::Endpoint as SendMessageEventEndpoint;
use ruma_identifiers::{RoomId, UserId};
use serde_json::to_string;

#[test]
fn successfully_forwards_a_text_message_from_rocketchat_to_matrix_when_the_user_is_not_registered_on_matrix() {
    let test = Test::new();
    let (message_forwarder, receiver) = MessageForwarder::new();
    let (invite_forwarder, invite_receiver) = handlers::MatrixInviteUser::with_forwarder(test.config.as_url.clone());
    let (join_forwarder, join_receiver) = handlers::MatrixJoinRoom::with_forwarder(test.config.as_url.clone());
    let (set_display_name_forwarder, set_display_name_receiver) = MessageForwarder::new();
    let mut matrix_router = test.default_matrix_routes();
    matrix_router.put(SendMessageEventEndpoint::router_path(), message_forwarder, "send_message_event");
    matrix_router.post(InviteUserEndpoint::router_path(), invite_forwarder, "invite_user");
    matrix_router.post(JoinRoomByIdEndpoint::router_path(), join_forwarder, "join_room");
    matrix_router.put(SetDisplayNameEndpoint::router_path(), set_display_name_forwarder, "set_display_name");

    let test = test.with_matrix_routes(matrix_router)
        .with_rocketchat_mock()
        .with_connected_admin_room()
        .with_logged_in_user()
        .with_bridged_room(("spec_channel", "spec_user"))
        .run();

    let message = Message {
        message_id: "spec_id".to_string(),
        token: Some(RS_TOKEN.to_string()),
        channel_id: "spec_channel_id".to_string(),
        channel_name: Some("spec_channel".to_string()),
        user_id: "new_user_id".to_string(),
        user_name: "new_spec_user".to_string(),
        text: "spec_message".to_string(),
    };
    let payload = to_string(&message).unwrap();

    helpers::simulate_message_from_rocketchat(&test.config.as_url, &payload);

    // discard admin room invite
    invite_receiver.recv_timeout(default_timeout()).unwrap();
    // receive the invite messages
    let spec_user_invite_message = invite_receiver.recv_timeout(default_timeout()).unwrap();
    assert!(spec_user_invite_message.contains("@spec_user:localhost"));
    let virtual_spec_user_invite_message = invite_receiver.recv_timeout(default_timeout()).unwrap();
    assert!(virtual_spec_user_invite_message.contains("@rocketchat_spec_user_id_rc_id:localhost"));
    let new_user_invite_message = invite_receiver.recv_timeout(default_timeout()).unwrap();
    assert!(new_user_invite_message.contains("@rocketchat_new_user_id_rc_id:localhost"));

    // receive the join message
    assert!(join_receiver.recv_timeout(default_timeout()).is_ok());

    // receive set display
    let set_display_name_spec_user = set_display_name_receiver.recv_timeout(default_timeout()).unwrap();
    assert!(set_display_name_spec_user.contains("spec_user"));
    let set_display_name_new_spec_user = set_display_name_receiver.recv_timeout(default_timeout()).unwrap();
    assert!(set_display_name_new_spec_user.contains("new_spec_user"));

    // discard welcome message
    receiver.recv_timeout(default_timeout()).unwrap();
    // discard connect message
    receiver.recv_timeout(default_timeout()).unwrap();
    // discard login message
    receiver.recv_timeout(default_timeout()).unwrap();
    // discard room bridged message
    receiver.recv_timeout(default_timeout()).unwrap();

    let message_received_by_matrix = receiver.recv_timeout(default_timeout()).unwrap();
    assert!(message_received_by_matrix.contains("spec_message"));

    // the bot, the user who bridged the channel and two virtual user are in the channel
    let matrix_api = MatrixApi::new(&test.config, DEFAULT_LOGGER.clone()).unwrap();
    let user_ids = Room::user_ids(&(*matrix_api), RoomId::try_from("!spec_channel_id:localhost").unwrap(), None).unwrap();

    assert_eq!(user_ids.len(), 4);

    let bot_user_id = UserId::try_from("@rocketchat:localhost").unwrap();
    let spec_user_id = UserId::try_from("@spec_user:localhost").unwrap();
    let virtual_spec_user = UserId::try_from("@rocketchat_spec_user_id_rc_id:localhost").unwrap();
    let users_iter = user_ids.into_iter();
    let user_ids = users_iter
        .filter_map(|id| if id != bot_user_id && id != spec_user_id && id != virtual_spec_user {
            Some(id)
        } else {
            None
        })
        .collect::<Vec<UserId>>();
    let new_user_id = user_ids.iter().next().unwrap();

    // the virtual user was create with the Rocket.Chat user ID
    let connection = test.connection_pool.get().unwrap();
    let user_on_rocketchat = UserOnRocketchatServer::find(&connection, new_user_id, "rc_id".to_string()).unwrap();
    assert_eq!(user_on_rocketchat.rocketchat_user_id.unwrap(), "new_user_id".to_string());

    let second_message = Message {
        message_id: "spec_id_2".to_string(),
        token: Some(RS_TOKEN.to_string()),
        channel_id: "spec_channel_id".to_string(),
        channel_name: Some("spec_channel".to_string()),
        user_id: "new_user_id".to_string(),
        user_name: "new_spec_user".to_string(),
        text: "spec_message 2".to_string(),
    };
    let second_payload = to_string(&second_message).unwrap();

    helpers::simulate_message_from_rocketchat(&test.config.as_url, &second_payload);

    // make sure the display name is only set once
    assert!(set_display_name_receiver.recv_timeout(default_timeout()).is_err());

    let message_received_by_matrix = receiver.recv_timeout(default_timeout()).unwrap();
    assert!(message_received_by_matrix.contains("spec_message 2"));
}

#[test]
fn successfully_forwards_a_text_message_from_rocketchat_to_matrix_when_the_user_is_registered_on_matrix() {
    let test = Test::new();
    let (message_forwarder, receiver) = MessageForwarder::new();
    let (invite_forwarder, invite_receiver) = handlers::MatrixInviteUser::with_forwarder(test.config.as_url.clone());
    let (join_forwarder, join_receiver) = handlers::MatrixJoinRoom::with_forwarder(test.config.as_url.clone());
    let (set_display_name_forwarder, set_display_name_receiver) = MessageForwarder::new();
    let mut matrix_router = test.default_matrix_routes();
    matrix_router.put(SendMessageEventEndpoint::router_path(), message_forwarder, "send_message_event");
    matrix_router.post(InviteUserEndpoint::router_path(), invite_forwarder, "invite_user");
    matrix_router.post(JoinRoomByIdEndpoint::router_path(), join_forwarder, "join_room");
    matrix_router.put(SetDisplayNameEndpoint::router_path(), set_display_name_forwarder, "set_display_name");


    let test = test.with_matrix_routes(matrix_router)
        .with_rocketchat_mock()
        .with_connected_admin_room()
        .with_logged_in_user()
        .with_bridged_room(("spec_channel", "spec_user"))
        .run();

    let message = Message {
        message_id: "spec_id".to_string(),
        token: Some(RS_TOKEN.to_string()),
        channel_id: "spec_channel_id".to_string(),
        channel_name: Some("spec_channel".to_string()),
        user_id: "spec_user_id".to_string(),
        user_name: "spec_user".to_string(),
        text: "spec_message".to_string(),
    };
    let payload = to_string(&message).unwrap();

    helpers::simulate_message_from_rocketchat(&test.config.as_url, &payload);

    // discard admin room invite
    invite_receiver.recv_timeout(default_timeout()).unwrap();
    // receive the invite messages
    let spec_user_invite_message = invite_receiver.recv_timeout(default_timeout()).unwrap();
    assert!(spec_user_invite_message.contains("@spec_user:localhost"));
    let virtual_user_invite_message = invite_receiver.recv_timeout(default_timeout()).unwrap();
    assert!(virtual_user_invite_message.contains("@rocketchat_spec_user_id_rc_id:localhost"));

    // discard admin room join
    join_receiver.recv_timeout(default_timeout()).unwrap();
    // receive the join message
    assert!(join_receiver.recv_timeout(default_timeout()).is_ok());

    // receive set display name
    let set_display_name_spec_user = set_display_name_receiver.recv_timeout(default_timeout()).unwrap();
    assert!(set_display_name_spec_user.contains("spec_user"));

    // discard welcome message
    receiver.recv_timeout(default_timeout()).unwrap();
    // discard connect message
    receiver.recv_timeout(default_timeout()).unwrap();
    // discard login message
    receiver.recv_timeout(default_timeout()).unwrap();
    // discard room bridged message
    receiver.recv_timeout(default_timeout()).unwrap();

    let message_received_by_matrix = receiver.recv_timeout(default_timeout()).unwrap();
    assert!(message_received_by_matrix.contains("spec_message"));

    // the bot, the user who bridged the channel and the virtual user are in the channel
    let matrix_api = MatrixApi::new(&test.config, DEFAULT_LOGGER.clone()).unwrap();
    let user_ids = Room::user_ids(&(*matrix_api), RoomId::try_from("!spec_channel_id:localhost").unwrap(), None).unwrap();
    assert_eq!(user_ids.len(), 3);

    let bot_user_id = UserId::try_from("@rocketchat:localhost").unwrap();
    let spec_user_id = UserId::try_from("@spec_user:localhost").unwrap();
    let user_ids_iter = user_ids.into_iter();
    let user_ids = user_ids_iter
        .filter_map(|id| if id != bot_user_id && id != spec_user_id {
            Some(id.clone())
        } else {
            None
        })
        .collect::<Vec<UserId>>();
    let new_user_id = user_ids.iter().next().unwrap();

    // the virtual user was create with the Rocket.Chat user ID because the exiting matrix user
    // cannot be used since the application service can only impersonate virtual users.
    let connection = test.connection_pool.get().unwrap();
    let user_on_rocketchat = UserOnRocketchatServer::find(&connection, new_user_id, "rc_id".to_string()).unwrap();
    assert_eq!(user_on_rocketchat.rocketchat_user_id.unwrap(), "spec_user_id".to_string());

    let second_message = Message {
        message_id: "spec_id_2".to_string(),
        token: Some(RS_TOKEN.to_string()),
        channel_id: "spec_channel_id".to_string(),
        channel_name: Some("spec_channel".to_string()),
        user_id: "spec_user_id".to_string(),
        user_name: "spec_user".to_string(),
        text: "spec_message 2".to_string(),
    };
    let second_payload = to_string(&second_message).unwrap();

    helpers::simulate_message_from_rocketchat(&test.config.as_url, &second_payload);

    // make sure the display name is only set once
    assert!(set_display_name_receiver.recv_timeout(default_timeout()).is_err());

    let message_received_by_matrix = receiver.recv_timeout(default_timeout()).unwrap();
    assert!(message_received_by_matrix.contains("spec_message 2"));
}

#[test]
fn update_the_display_name_when_the_user_changed_it_on_the_rocketchat_server() {
    let test = Test::new();
    let (set_display_name_forwarder, set_display_name_receiver) = MessageForwarder::new();
    let mut matrix_router = test.default_matrix_routes();
    matrix_router.put(SetDisplayNameEndpoint::router_path(), set_display_name_forwarder, "set_display_name");

    let test = test.with_matrix_routes(matrix_router)
        .with_rocketchat_mock()
        .with_connected_admin_room()
        .with_logged_in_user()
        .with_bridged_room(("spec_channel", "spec_user"))
        .run();

    let message = Message {
        message_id: "spec_id".to_string(),
        token: Some(RS_TOKEN.to_string()),
        channel_id: "spec_channel_id".to_string(),
        channel_name: Some("spec_channel".to_string()),
        user_id: "virtual_spec_user_id".to_string(),
        user_name: "virtual_spec_user".to_string(),
        text: "spec_message".to_string(),
    };
    let payload = to_string(&message).unwrap();

    helpers::simulate_message_from_rocketchat(&test.config.as_url, &payload);

    let spec_user_display_name_message = set_display_name_receiver.recv_timeout(default_timeout()).unwrap();
    assert!(spec_user_display_name_message.contains("spec_user"));
    let virtual_spec_user_display_name_message = set_display_name_receiver.recv_timeout(default_timeout()).unwrap();
    assert!(virtual_spec_user_display_name_message.contains("virtual_spec_user"));

    let second_message_with_new_username = Message {
        message_id: "spec_id_2".to_string(),
        token: Some(RS_TOKEN.to_string()),
        channel_id: "spec_channel_id".to_string(),
        channel_name: Some("spec_channel".to_string()),
        user_id: "virtual_spec_user_id".to_string(),
        user_name: "virtual_spec_user_new".to_string(),
        text: "spec_message 2".to_string(),
    };
    let second_payload_with_new_username = to_string(&second_message_with_new_username).unwrap();

    helpers::simulate_message_from_rocketchat(&test.config.as_url, &second_payload_with_new_username);

    let new_display_name_message = set_display_name_receiver.recv_timeout(default_timeout()).unwrap();
    assert!(new_display_name_message.contains("spec_user_new"));

    let connection = test.connection_pool.get().unwrap();
    let matrix_api = MatrixApi::new(&test.config, DEFAULT_LOGGER.clone()).unwrap();
    let rocketchat_server = Room::rocketchat_server_for_admin_room(
        &connection,
        matrix_api.as_ref(),
        RoomId::try_from("!admin_room_id:localhost").unwrap(),
    ).unwrap()
        .unwrap();
    let user_on_rocketchat_server = UserOnRocketchatServer::find_by_rocketchat_user_id(
        &connection,
        rocketchat_server.id,
        "virtual_spec_user_id".to_string(),
        true,
    ).unwrap()
        .unwrap();
    assert_eq!(user_on_rocketchat_server.rocketchat_username.unwrap(), "virtual_spec_user_new".to_string());
}

#[test]
fn message_is_forwarded_even_if_setting_the_display_name_failes() {
    let test = Test::new();
    let (message_forwarder, receiver) = MessageForwarder::new();
    let mut matrix_router = test.default_matrix_routes();
    matrix_router.put(SendMessageEventEndpoint::router_path(), message_forwarder, "send_message_event");
    matrix_router.put(
        SetDisplayNameEndpoint::router_path(),
        handlers::MatrixErrorResponder {
            status: status::InternalServerError,
            message: "Could not set display name".to_string(),
        },
        "set_display_name",
    );

    let test = test.with_matrix_routes(matrix_router)
        .with_rocketchat_mock()
        .with_connected_admin_room()
        .with_logged_in_user()
        .with_bridged_room(("spec_channel", "spec_user"))
        .run();

    let message = Message {
        message_id: "spec_id".to_string(),
        token: Some(RS_TOKEN.to_string()),
        channel_id: "spec_channel_id".to_string(),
        channel_name: Some("spec_channel".to_string()),
        user_id: "virtual_spec_user_id".to_string(),
        user_name: "virtual_spec_user".to_string(),
        text: "spec_message".to_string(),
    };
    let payload = to_string(&message).unwrap();

    helpers::simulate_message_from_rocketchat(&test.config.as_url, &payload);

    // discard welcome message
    receiver.recv_timeout(default_timeout()).unwrap();
    // discard connect message
    receiver.recv_timeout(default_timeout()).unwrap();
    // discard login message
    receiver.recv_timeout(default_timeout()).unwrap();
    // discard room bridged message
    receiver.recv_timeout(default_timeout()).unwrap();

    let message_received_by_matrix = receiver.recv_timeout(default_timeout()).unwrap();
    assert!(message_received_by_matrix.contains("spec_message"));
}

#[test]
fn rocketchat_sends_mal_formatted_json() {
    let test = Test::new().run();
    let payload = "bad_json";

    let url = format!("{}/rocketchat", &test.config.as_url);

    let params = HashMap::new();
    let (_, status_code) = RestApi::call(Method::Post, &url, payload, &params, None).unwrap();

    assert_eq!(status_code, StatusCode::UnprocessableEntity)
}

#[test]
fn no_message_is_forwarded_when_inviting_the_user_failes() {
    let test = Test::new();
    let (message_forwarder, receiver) = MessageForwarder::new();
    let mut matrix_router = test.default_matrix_routes();
    matrix_router.put(SendMessageEventEndpoint::router_path(), message_forwarder, "send_message_event");
    let invite_handler = handlers::MatrixInviteUser { as_url: test.config.as_url.clone() };
    let conditional_error = handlers::MatrixConditionalErrorResponder {
        status: status::InternalServerError,
        message: "Could not invite user".to_string(),
        conditional_content: "new_user",
    };
    let mut invite_with_error = Chain::new(invite_handler);
    invite_with_error.link_before(conditional_error);
    matrix_router.post(InviteUserEndpoint::router_path(), invite_with_error, "invite_user_spec_channel");


    let test = test.with_matrix_routes(matrix_router)
        .with_rocketchat_mock()
        .with_connected_admin_room()
        .with_logged_in_user()
        .with_bridged_room(("spec_channel", "spec_user"))
        .run();

    let message = Message {
        message_id: "spec_id".to_string(),
        token: Some(RS_TOKEN.to_string()),
        channel_id: "spec_channel_id".to_string(),
        channel_name: Some("spec_channel".to_string()),
        user_id: "new_user_id".to_string(),
        user_name: "new_spec_user".to_string(),
        text: "spec_message".to_string(),
    };
    let payload = to_string(&message).unwrap();

    // discard welcome message
    receiver.recv_timeout(default_timeout()).unwrap();
    // discard connect message
    receiver.recv_timeout(default_timeout()).unwrap();
    // discard login message
    receiver.recv_timeout(default_timeout()).unwrap();
    // discard room bridged message
    receiver.recv_timeout(default_timeout()).unwrap();

    helpers::simulate_message_from_rocketchat(&test.config.as_url, &payload);

    // no message is forwarded
    assert!(receiver.recv_timeout(default_timeout()).is_err());
}

#[test]
fn ignore_messages_to_a_room_that_is_not_bridged() {
    let test = Test::new();
    let (message_forwarder, receiver) = MessageForwarder::new();
    let mut matrix_router = test.default_matrix_routes();
    matrix_router.put(SendMessageEventEndpoint::router_path(), message_forwarder, "send_message_event");

    let test =
        test.with_matrix_routes(matrix_router).with_rocketchat_mock().with_connected_admin_room().with_logged_in_user().run();

    let message = Message {
        message_id: "spec_id".to_string(),
        token: Some(RS_TOKEN.to_string()),
        channel_id: "not_bridged_channel_id".to_string(),
        channel_name: Some("not_bridged_channel".to_string()),
        user_id: "new_user_id".to_string(),
        user_name: "new_spec_user".to_string(),
        text: "spec_message".to_string(),
    };
    let payload = to_string(&message).unwrap();

    // discard welcome message
    receiver.recv_timeout(default_timeout()).unwrap();
    // discard connect message
    receiver.recv_timeout(default_timeout()).unwrap();
    // discard login message
    receiver.recv_timeout(default_timeout()).unwrap();

    helpers::simulate_message_from_rocketchat(&test.config.as_url, &payload);

    // no message is forwarded
    assert!(receiver.recv_timeout(default_timeout()).is_err());
}

#[test]
fn ignore_messages_forwarded_from_rocketchat_if_the_non_virtual_user_just_sent_a_message_on_matrix_to_avoid_loops() {
    let test = Test::new();
    let (message_forwarder, receiver) = MessageForwarder::new();
    let mut matrix_router = test.default_matrix_routes();
    matrix_router.put(SendMessageEventEndpoint::router_path(), message_forwarder, "send_message_event");

    let test = test.with_matrix_routes(matrix_router)
        .with_rocketchat_mock()
        .with_connected_admin_room()
        .with_logged_in_user()
        .with_bridged_room(("spec_channel", "spec_user"))
        .run();

    helpers::send_room_message_from_matrix(
        &test.config.as_url,
        RoomId::try_from("!spec_channel_id:localhost").unwrap(),
        UserId::try_from("@spec_user:localhost").unwrap(),
        "message from Matrix".to_string(),
    );

    let message = Message {
        message_id: "spec_id".to_string(),
        token: Some(RS_TOKEN.to_string()),
        channel_id: "spec_channel_id".to_string(),
        channel_name: Some("spec_channel".to_string()),
        user_id: "spec_user_id".to_string(),
        user_name: "spec_user".to_string(),
        text: "spec_message".to_string(),
    };
    let payload = to_string(&message).unwrap();

    helpers::simulate_message_from_rocketchat(&test.config.as_url, &payload);

    // discard welcome message
    receiver.recv_timeout(default_timeout()).unwrap();
    // discard connect message
    receiver.recv_timeout(default_timeout()).unwrap();
    // discard login message
    receiver.recv_timeout(default_timeout()).unwrap();
    // discard room bridged message
    receiver.recv_timeout(default_timeout()).unwrap();

    assert!(receiver.recv_timeout(default_timeout()).is_err());
}

#[test]
fn do_not_forward_messages_when_the_channel_was_bridged_but_is_unbridged_now() {
    let test = Test::new();
    let (message_forwarder, receiver) = MessageForwarder::new();
    let mut matrix_router = test.default_matrix_routes();
    matrix_router.put(SendMessageEventEndpoint::router_path(), message_forwarder, "send_message_event");
    matrix_router.put(
        SetDisplayNameEndpoint::router_path(),
        handlers::MatrixErrorResponder {
            status: status::InternalServerError,
            message: "Could not set display name".to_string(),
        },
        "set_display_name",
    );

    let test = test.with_matrix_routes(matrix_router)
        .with_rocketchat_mock()
        .with_connected_admin_room()
        .with_logged_in_user()
        .with_bridged_room(("spec_channel", "spec_user"))
        .run();

    helpers::leave_room(
        &test.config,
        RoomId::try_from("!spec_channel_id:localhost").unwrap(),
        UserId::try_from("@spec_user:localhost").unwrap(),
    );

    helpers::send_room_message_from_matrix(
        &test.config.as_url,
        RoomId::try_from("!admin_room_id:localhost").unwrap(),
        UserId::try_from("@spec_user:localhost").unwrap(),
        "unbridge spec_channel".to_string(),
    );

    let message = Message {
        message_id: "spec_id".to_string(),
        token: Some(RS_TOKEN.to_string()),
        channel_id: "spec_channel_id".to_string(),
        channel_name: Some("spec_channel".to_string()),
        user_id: "virtual_spec_user_id".to_string(),
        user_name: "virtual_spec_user".to_string(),
        text: "spec_message".to_string(),
    };
    let payload = to_string(&message).unwrap();

    helpers::simulate_message_from_rocketchat(&test.config.as_url, &payload);

    // discard welcome message
    receiver.recv_timeout(default_timeout()).unwrap();
    // discard connect message
    receiver.recv_timeout(default_timeout()).unwrap();
    // discard login message
    receiver.recv_timeout(default_timeout()).unwrap();
    // discard room bridged message
    receiver.recv_timeout(default_timeout()).unwrap();
    // discard room unbridged message
    receiver.recv_timeout(default_timeout()).unwrap();

    assert!(receiver.recv_timeout(default_timeout()).is_err());
}

#[test]
fn returns_unauthorized_when_the_rs_token_is_missing() {
    let test = Test::new().run();
    let message = Message {
        message_id: "spec_id".to_string(),
        token: None,
        channel_id: "spec_channel_id".to_string(),
        channel_name: Some("spec_channel".to_string()),
        user_id: "virtual_spec_user_id".to_string(),
        user_name: "virtual_spec_user".to_string(),
        text: "spec_message".to_string(),
    };
    let payload = to_string(&message).unwrap();

    let url = format!("{}/rocketchat", &test.config.as_url);

    let params = HashMap::new();
    let (_, status_code) = RestApi::call(Method::Post, &url, &payload, &params, None).unwrap();

    assert_eq!(status_code, StatusCode::Unauthorized)
}

#[test]
fn returns_forbidden_when_the_rs_token_does_not_match_a_server() {
    let test = Test::new().run();
    let message = Message {
        message_id: "spec_id".to_string(),
        token: Some("wrong_token".to_string()),
        channel_id: "spec_channel_id".to_string(),
        channel_name: Some("spec_channel".to_string()),
        user_id: "virtual_spec_user_id".to_string(),
        user_name: "virtual_spec_user".to_string(),
        text: "spec_message".to_string(),
    };
    let payload = to_string(&message).unwrap();

    let url = format!("{}/rocketchat", &test.config.as_url);

    let params = HashMap::new();
    let (_, status_code) = RestApi::call(Method::Post, &url, &payload, &params, None).unwrap();

    assert_eq!(status_code, StatusCode::Forbidden)
}
