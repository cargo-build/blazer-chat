#![allow(clippy::unwrap_used)] // tests assert the expected path; failures panic via expect
//! Protocol tests: the client speaks canned Rocket.Chat responses through a
//! fake transport — no sockets, no server. Fixtures mirror the shapes in the
//! vendored server source (`rooms.get`, `channels.history`,
//! `chat.sendMessage`, `login`).

use super::*;
use crate::http::{Request, Response, Transport};
use std::cell::RefCell;

/// A transport that answers calls per path (first matching needle) and
/// records the last request for assertions. Clones share the record, so a
/// test can keep a handle while the client owns another clone.
#[derive(Clone)]
struct FakeTransport {
    routes: std::rc::Rc<Vec<(String, u16, String)>>,
    last_request: std::rc::Rc<RefCell<Option<String>>>,
}

impl FakeTransport {
    /// One route per needle: every request whose path contains the needle
    /// gets this status/body.
    fn answering(routes: &[(&str, u16, &str)]) -> Self {
        Self {
            routes: std::rc::Rc::new(
                routes
                    .iter()
                    .map(|(needle, status, body)| {
                        ((*needle).to_owned(), *status, (*body).to_owned())
                    })
                    .collect(),
            ),
            last_request: std::rc::Rc::new(RefCell::new(None)),
        }
    }

    fn answer_for(&self, path: &str) -> (u16, &str) {
        self.routes
            .iter()
            .find(|(needle, _, _)| path.contains(needle))
            .map(|(_, status, body)| (*status, body.as_str()))
            .expect("no route matched the request path")
    }

    fn last_request(&self) -> String {
        self.last_request
            .borrow()
            .clone()
            .expect("a request was made")
    }
}

impl Transport for FakeTransport {
    async fn call(&self, request: Request<'_>) -> std::io::Result<Response> {
        *self.last_request.borrow_mut() = Some(format!(
            "{} {} {} {}",
            request.method,
            request.path,
            request
                .headers
                .iter()
                .map(|(name, value)| format!("{name}:{value}"))
                .collect::<Vec<_>>()
                .join(" "),
            request.body.as_deref().unwrap_or_default()
        ));
        let (status, body) = self.answer_for(request.path);
        Ok(Response {
            status,
            body: body.to_owned(),
        })
    }
}

const LOGIN_BODY: &str = r#"{"status":"success","data":{"userId":"u1","authToken":"tok","me":{"username":"tester","name":"Tester","emails":null}}}"#;
const ROOMS_BODY: &str = r#"{"update":[
    {"_id":"GENERAL","name":"general","t":"c","lastMessage":{}},
    {"_id":"priv1","name":"secret","t":"p"},
    {"_id":"dm1","t":"d","usernames":["tester","alice"]},
    {"_id":"selfdm","t":"d","usernames":["tester"]}
],"remove":[],"success":true}"#;
const HISTORY_BODY: &str = r#"{"messages":[
    {"_id":"m2","rid":"GENERAL","msg":"second","ts":"2026-11-07T15:00:02Z","u":{"username":"alice"}},
    {"_id":"m1","rid":"GENERAL","msg":"first","ts":"2026-11-07T15:00:01Z","u":{"username":"tester"}}
],"success":true,"unreadNotLoaded":0}"#;
const SEND_BODY: &str = r#"{"message":{"_id":"m3","rid":"GENERAL","msg":"hello","ts":"2026-11-07T15:00:03Z","u":{"username":"tester"},"channels":[]},"success":true}"#;
const UPDATE_BODY: &str = r#"{"message":{"_id":"m3","rid":"GENERAL","msg":"edited","ts":"2026-11-07T15:00:03Z","u":{"username":"tester"}},"success":true}"#;
const REACT_BODY: &str = r#"{"success":true}"#;
const NOT_ALLOWED_BODY: &str =
    r#"{"success":false,"error":"Not allowed [error-action-not-allowed]"}"#;
const UNAUTHORIZED_BODY: &str =
    r#"{"status":"error","error":"Unauthorized","message":"Unauthorized"}"#;

fn client(transport: FakeTransport) -> RcClient<FakeTransport> {
    RcClient::with_transport(RcSettings::default(), transport)
}

fn authenticated_routes() -> Vec<(&'static str, u16, &'static str)> {
    vec![
        ("login", 200, LOGIN_BODY),
        ("rooms.get", 200, ROOMS_BODY),
        ("channels.history", 200, HISTORY_BODY),
        ("groups.history", 200, HISTORY_BODY),
        ("im.history", 200, HISTORY_BODY),
        ("chat.sendMessage", 200, SEND_BODY),
        ("chat.update", 200, UPDATE_BODY),
        ("chat.react", 200, REACT_BODY),
        ("chat.delete", 400, NOT_ALLOWED_BODY),
    ]
}

#[test]
fn login_stores_the_session_and_sends_the_documented_body() {
    let transport = FakeTransport::answering(&[("login", 200, LOGIN_BODY)]);
    let mut rc = client(transport.clone());

    let session = futures_lite::future::block_on(rc.login("tester", "secret")).unwrap();
    assert_eq!(session.username, "tester");
    assert_eq!(session.auth_token, "tok");
    assert!(rc.session().is_some());

    let request = transport.last_request();
    assert!(request.contains("POST /api/v1/login"));
    assert!(request.contains(r#""user":"tester""#));
    assert!(request.contains(r#""password":"secret""#));
}

#[test]
fn rooms_maps_channel_private_and_direct() {
    let transport = FakeTransport::answering(&authenticated_routes());
    let mut rc = client(transport);
    futures_lite::future::block_on(rc.login("tester", "secret")).unwrap();

    let rooms = futures_lite::future::block_on(rc.rooms()).unwrap();
    assert_eq!(rooms.len(), 4);
    assert_eq!(rooms[0].name, "general");
    assert_eq!(rooms[0].kind, 'c');
    assert_eq!(rooms[1].kind, 'p');
    // DMs display the other participant; a self-DM shows the own username.
    assert_eq!(rooms[2].name, "alice");
    assert_eq!(rooms[2].kind, 'd');
    assert_eq!(rooms[3].name, "tester");
    assert_eq!(rooms[3].kind, 'd');
}

#[test]
fn history_returns_oldest_first_with_auth_headers() {
    let transport = FakeTransport::answering(&authenticated_routes());
    let mut rc = client(transport.clone());
    futures_lite::future::block_on(rc.login("tester", "secret")).unwrap();

    let messages = futures_lite::future::block_on(rc.history("GENERAL")).unwrap();
    assert_eq!(messages.len(), 2);
    assert_eq!(messages[0].text, "first");
    assert_eq!(messages[1].text, "second");
    assert_eq!(messages[0].sender, "tester");

    let request = transport.last_request();
    assert!(request.contains("GET /api/v1/channels.history?roomId=GENERAL"));
    assert!(request.contains("x-auth-token:tok"));
    assert!(request.contains("x-user-id:u1"));
}

#[test]
fn send_speaks_the_msg_field() {
    let transport = FakeTransport::answering(&authenticated_routes());
    let mut rc = client(transport.clone());
    futures_lite::future::block_on(rc.login("tester", "secret")).unwrap();

    let message = futures_lite::future::block_on(rc.send("GENERAL", "hello")).unwrap();
    assert_eq!(message.text, "hello");
    assert_eq!(message.room_id, "GENERAL");

    let request = transport.last_request();
    assert!(request.contains("POST /api/v1/chat.sendMessage"));
    assert!(request.contains(r#""rid":"GENERAL""#));
    assert!(request.contains(r#""msg":"hello""#));
}

#[test]
fn update_edits_and_react_toggles_with_the_documented_bodies() {
    let transport = FakeTransport::answering(&authenticated_routes());
    let mut rc = client(transport.clone());
    futures_lite::future::block_on(rc.login("tester", "secret")).unwrap();

    let edited =
        futures_lite::future::block_on(rc.update_message("GENERAL", "m3", "edited")).unwrap();
    assert_eq!(edited.text, "edited");
    assert_eq!(edited.id, "m3");
    let request = transport.last_request();
    assert!(request.contains("POST /api/v1/chat.update"));
    assert!(request.contains(r#""roomId":"GENERAL""#));
    assert!(request.contains(r#""msgId":"m3""#));
    assert!(request.contains(r#""text":"edited""#));

    futures_lite::future::block_on(rc.react("m3", "tada")).unwrap();
    let request = transport.last_request();
    // The reaction endpoint takes messageId (not msgId) and the bare
    // emoji name — live-verified against RC 8.5.
    assert!(request.contains(r#""messageId":"m3""#));
    assert!(request.contains(r#""emoji":"tada""#));
}

#[test]
fn delete_surfaces_the_server_refusal_as_an_http_error() {
    // The school server denies deletions account-wide
    // (error-action-not-allowed) even for the author's own messages.
    let transport = FakeTransport::answering(&authenticated_routes());
    let mut rc = client(transport.clone());
    futures_lite::future::block_on(rc.login("tester", "secret")).unwrap();

    let error = futures_lite::future::block_on(rc.delete_message("GENERAL", "m3")).unwrap_err();
    assert!(matches!(error, RcError::Http { status: 400, .. }));
}

#[test]
fn history_routes_by_room_kind() {
    let transport = FakeTransport::answering(&authenticated_routes());
    let mut rc = client(transport.clone());
    futures_lite::future::block_on(rc.login("tester", "secret")).unwrap();
    futures_lite::future::block_on(rc.rooms()).unwrap();

    // priv1 is a group ('p'), dm1 is direct ('d') in the rooms fixture.
    futures_lite::future::block_on(rc.history("priv1")).unwrap();
    assert!(
        transport
            .last_request()
            .contains("GET /api/v1/groups.history?roomId=priv1")
    );

    futures_lite::future::block_on(rc.history("dm1")).unwrap();
    assert!(
        transport
            .last_request()
            .contains("GET /api/v1/im.history?roomId=dm1")
    );

    futures_lite::future::block_on(rc.history("GENERAL")).unwrap();
    assert!(
        transport
            .last_request()
            .contains("GET /api/v1/channels.history?roomId=GENERAL")
    );
}

#[test]
fn unauthenticated_calls_refuse_before_the_wire() {
    let transport = FakeTransport::answering(&authenticated_routes());
    let mut rc = client(transport);
    match futures_lite::future::block_on(rc.history("GENERAL")) {
        Err(RcError::NoSession) => {}
        other => panic!("expected NoSession, got {other:?}"),
    }
}

#[test]
fn server_errors_carry_status_and_body() {
    let transport = FakeTransport::answering(&[
        ("login", 200, LOGIN_BODY),
        ("rooms.get", 401, UNAUTHORIZED_BODY),
    ]);
    let mut rc = client(transport);
    futures_lite::future::block_on(rc.login("tester", "secret")).unwrap();
    match futures_lite::future::block_on(rc.rooms()) {
        Err(RcError::Http { status, body }) => {
            assert_eq!(status, 401);
            assert!(body.contains("Unauthorized"));
        }
        other => panic!("expected Http error, got {other:?}"),
    }
}
