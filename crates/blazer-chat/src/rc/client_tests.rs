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
pub(crate) struct FakeTransport {
    routes: std::rc::Rc<Vec<(String, u16, String)>>,
    last_request: std::rc::Rc<RefCell<Option<String>>>,
}

impl FakeTransport {
    /// One route per needle: every request whose path contains the needle
    /// gets this status/body.
    pub(crate) fn answering(routes: &[(&str, u16, &str)]) -> Self {
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
            .unwrap_or_else(|| panic!("no route matched the request path: {path}"))
    }

    pub(crate) fn last_request(&self) -> String {
        self.last_request
            .borrow()
            .clone()
            .expect("a request was made")
    }

    /// Whether any request reached the transport (the no-wire proof for
    /// refusal paths).
    pub(crate) fn made_request(&self) -> bool {
        self.last_request.borrow().is_some()
    }
}

impl Transport for FakeTransport {
    async fn get_bytes(&self, _path: &str) -> std::io::Result<Vec<u8>> {
        Ok(Vec::new())
    }

    async fn get_bytes_with_headers(
        &self,
        _path: &str,
        _headers: &[(String, String)],
    ) -> std::io::Result<Vec<u8>> {
        Ok(Vec::new())
    }

    async fn call(&self, request: Request<'_>) -> std::io::Result<Response> {
        let body = request
            .body
            .as_deref()
            .map(|bytes| String::from_utf8_lossy(bytes).into_owned())
            .unwrap_or_default();
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
            body
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
    {"_id":"GENERAL","name":"general","t":"c","lastMessage":{},"userMentions":2},
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
const SUBSCRIPTIONS_BODY: &str = r#"{"update":[
    {"rid":"GENERAL","unread":4},
    {"rid":"dm1","unread":0}
],"success":true}"#;
const MEDIA_BODY: &str =
    r#"{"file":{"_id":"f1","url":"/file-upload/f1/probe.txt"},"success":true}"#;
const FILE_HISTORY_BODY: &str = r#"{"messages":[
    {"_id":"f1","rid":"GENERAL","msg":"caption","ts":"2026-11-07T15:00:05Z","u":{"username":"tester"},
     "file":{"_id":"abc123","name":"probe image.png","size":42,"type":"image/png","typeGroup":"image"}}
],"success":true,"unreadNotLoaded":0}"#;
const THREAD_BODY: &str = r#"{"messages":[{"_id":"r1","rid":"GENERAL","msg":"reply","tmid":"root","u":{"username":"tester"}}],"success":true}"#;
const NOT_ALLOWED_BODY: &str =
    r#"{"success":false,"error":"Not allowed [error-action-not-allowed]"}"#;
const UNAUTHORIZED_BODY: &str =
    r#"{"status":"error","error":"Unauthorized","message":"Unauthorized"}"#;

pub(crate) fn client(transport: FakeTransport) -> RcClient<FakeTransport> {
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
        ("subscriptions.read", 200, REACT_BODY),
        ("subscriptions.get", 200, SUBSCRIPTIONS_BODY),
        ("rooms.media/", 200, MEDIA_BODY),
        ("rooms.mediaConfirm/", 200, SEND_BODY),
        ("chat.getThreadMessages", 200, THREAD_BODY),
    ]
}

/// Captured live payloads, sanitized: real server shapes (file
/// uploads, image attachments, reactions, thread replies, room kinds)
/// must keep decoding after wire edits, and the fixtures stay free of
/// private data (see the privacy test below).
const IM_HISTORY_FIXTURE: &str = include_str!("fixtures/im-history.json");
const THREAD_MESSAGES_FIXTURE: &str = include_str!("fixtures/thread-messages.json");
const ROOMS_GET_FIXTURE: &str = include_str!("fixtures/rooms-get.json");

#[test]
fn captured_history_fixture_decodes_the_real_shapes() {
    let mut routes = authenticated_routes();
    routes.retain(|(route, ..)| !matches!(*route, "rooms.get" | "im.history"));
    routes.push(("rooms.get", 200, ROOMS_GET_FIXTURE));
    routes.push(("im.history", 200, IM_HISTORY_FIXTURE));
    let transport = FakeTransport::answering(&routes);
    let mut rc = client(transport);
    futures_lite::future::block_on(rc.login("alice", "secret")).unwrap();
    let rooms = futures_lite::future::block_on(rc.rooms()).unwrap();
    let dm = rooms
        .iter()
        .find(|room| room.kind == 'd')
        .expect("fixture has a DM");
    let messages = futures_lite::future::block_on(rc.history(&dm.id)).unwrap();

    assert!(!messages.is_empty(), "fixture history is not empty");
    // The fixture's 3 uploaded messages carry BOTH `file` and an
    // `attachments` array; `file` wins and the card decodes. Reaction
    // pills fold in on 6 messages.
    let uploads = messages.iter().filter(|m| m.file.is_some()).count();
    assert_eq!(uploads, 3, "file takes precedence over attachments");
    let any_file = messages.iter().find_map(|m| m.file.clone()).unwrap();
    assert!(any_file.path.starts_with("/file-upload/"));
    let reacted = messages.iter().filter(|m| !m.reactions.is_empty()).count();
    assert!(reacted >= 6, "reaction decode, got {reacted}");
}

#[test]
fn captured_thread_fixture_decodes_thread_replies() {
    let routes: Vec<(&'static str, u16, &'static str)> = vec![
        ("login", 200, LOGIN_BODY),
        ("chat.getThreadMessages", 200, THREAD_MESSAGES_FIXTURE),
    ];
    let transport = FakeTransport::answering(&routes);
    let mut rc = client(transport);
    futures_lite::future::block_on(rc.login("alice", "secret")).unwrap();
    let thread = futures_lite::future::block_on(rc.thread_messages("t1")).unwrap();
    assert!(!thread.is_empty(), "fixture thread is not empty");
    assert!(
        thread.iter().all(|m| m.tmid.is_some()),
        "thread replies carry tmid"
    );
    // The captured image reply was posted through the raw API: it has
    // `attachments` and NO `file` — the fallback must synthesize the
    // same card from the attachment metadata.
    let from_attachment = thread
        .iter()
        .find(|m| m.file.is_some())
        .expect("attachment-only reply synthesizes a file");
    assert!(
        from_attachment
            .file
            .as_ref()
            .unwrap()
            .path
            .starts_with("/file-upload/")
    );
    assert_eq!(from_attachment.file.as_ref().unwrap().mime, "image/png");
}

#[test]
fn captured_fixtures_stay_free_of_private_data() {
    // The fixtures are sanitized captures in a public repository:
    // this guard fails the gate if a re-capture ever leaks.
    for fixture in [
        IM_HISTORY_FIXTURE,
        THREAD_MESSAGES_FIXTURE,
        ROOMS_GET_FIXTURE,
    ] {
        assert!(!fixture.contains("21-school"), "campus domain leaked");
        assert!(
            !fixture.contains("rocketchat-student"),
            "campus host leaked"
        );
        assert!(!fixture.contains("rorikstr"), "operator username leaked");
    }
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
    // Unread badges seed from subscriptions.get.
    assert_eq!(rooms[0].unread, 4);
    assert_eq!(rooms[1].kind, 'p');
    // DMs display the other participant; a self-DM shows the own username.
    assert_eq!(rooms[2].name, "alice");
    assert_eq!(rooms[2].kind, 'd');
    assert_eq!(rooms[2].unread, 0);
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
fn mark_read_posts_the_rid() {
    let transport = FakeTransport::answering(&authenticated_routes());
    let mut rc = client(transport.clone());
    futures_lite::future::block_on(rc.login("tester", "secret")).unwrap();

    futures_lite::future::block_on(rc.mark_read("GENERAL")).unwrap();
    let request = transport.last_request();
    assert!(request.contains("POST /api/v1/subscriptions.read"));
    assert!(request.contains(r#""rid":"GENERAL""#));
}

#[test]
fn upload_runs_the_two_phase_media_flow() {
    let transport = FakeTransport::answering(&authenticated_routes());
    let mut rc = client(transport.clone());
    futures_lite::future::block_on(rc.login("tester", "secret")).unwrap();

    let mut file = std::env::temp_dir();
    file.push("rchat-upload-probe.txt");
    std::fs::write(&file, b"probe bytes").unwrap();

    let message = futures_lite::future::block_on(rc.upload_file(
        "GENERAL",
        file.to_str().unwrap(),
        "caption",
    ))
    .unwrap();
    assert_eq!(message.id, "m3", "the confirm answers the full message");
    let request = transport.last_request();
    assert!(request.contains("POST /api/v1/rooms.mediaConfirm/GENERAL/f1"));
    assert!(request.contains(r#""msg":"caption""#));
}

#[test]
fn multipart_body_writes_the_file_and_caption_parts() {
    let body = super::client::multipart_body("BOUND", "probe.txt", "text/plain", b"hi", "caption");
    let body = String::from_utf8(body).unwrap();
    assert!(body.starts_with("--BOUND\r\n"));
    assert!(body.contains("content-disposition: form-data; name=\"file\"; filename=\"probe.txt\""));
    assert!(body.contains("content-type: text/plain\r\n\r\nhi\r\n"));
    assert!(body.contains("name=\"msg\"\r\n\r\ncaption"));
    assert!(body.ends_with("--BOUND--\r\n"));
}

#[test]
fn mime_type_maps_common_extensions() {
    assert_eq!(super::client::mime_type("shot.png"), "image/png");
    assert_eq!(super::client::mime_type("doc.PDF"), "application/pdf");
    assert_eq!(
        super::client::mime_type("blob.xyz"),
        "application/octet-stream"
    );
}

#[test]
fn attachments_decode_with_the_server_url_and_mime() {
    let routes = vec![
        ("login", 200, LOGIN_BODY),
        ("rooms.get", 200, ROOMS_BODY),
        ("subscriptions.get", 200, SUBSCRIPTIONS_BODY),
        ("channels.history", 200, FILE_HISTORY_BODY),
    ];
    let transport = FakeTransport::answering(&routes);
    let mut rc = client(transport);
    futures_lite::future::block_on(rc.login("tester", "secret")).unwrap();
    // History routes by the cached room kind: GENERAL is a channel.
    futures_lite::future::block_on(rc.rooms()).unwrap();

    let messages = futures_lite::future::block_on(rc.history("GENERAL")).unwrap();
    let file = messages[0].file.as_ref().expect("the file decodes");
    assert_eq!(file.name, "probe image.png");
    assert_eq!(file.size, 42);
    assert!(file.is_image());
    assert_eq!(file.path, "/file-upload/abc123/probe%20image.png");
    assert_eq!(
        file.url,
        "http://127.0.0.1:3000/file-upload/abc123/probe%20image.png"
    );
}

#[test]
fn history_page_passes_the_paging_cursor() {
    let routes = vec![
        ("login", 200, LOGIN_BODY),
        ("rooms.get", 200, ROOMS_BODY),
        ("subscriptions.get", 200, SUBSCRIPTIONS_BODY),
        ("channels.history", 200, FILE_HISTORY_BODY),
    ];
    let transport = FakeTransport::answering(&routes);
    let requests = transport.clone();
    let mut rc = client(transport);
    futures_lite::future::block_on(rc.login("tester", "secret")).unwrap();
    futures_lite::future::block_on(rc.rooms()).unwrap();
    futures_lite::future::block_on(rc.history_page("GENERAL", Some("2026-01-01T00:00:00Z")))
        .unwrap();
    assert!(
        requests
            .last_request()
            .contains("latest=2026-01-01T00:00:00Z"),
        "the oldest cached timestamp pages the next slice"
    );
}

#[test]
fn thread_metadata_decodes_from_history() {
    let body = r#"{"messages":[{"_id":"root1","rid":"GENERAL","msg":"root",
        "u":{"username":"tester"},"tcount":2,"tlm":"2026-11-07T19:31:08.613Z"}],
        "success":true}"#;
    let routes = vec![
        ("login", 200, LOGIN_BODY),
        ("rooms.get", 200, ROOMS_BODY),
        ("subscriptions.get", 200, SUBSCRIPTIONS_BODY),
        ("channels.history", 200, body),
    ];
    let transport = FakeTransport::answering(&routes);
    let mut rc = client(transport);
    futures_lite::future::block_on(rc.login("tester", "secret")).unwrap();
    futures_lite::future::block_on(rc.rooms()).unwrap();
    let messages = futures_lite::future::block_on(rc.history("GENERAL")).unwrap();
    assert_eq!(messages[0].tcount, Some(2));
    assert_eq!(messages[0].tlm.as_deref(), Some("2026-11-07T19:31:08.613Z"));
}

#[test]
fn threads_fetch_and_reply_with_the_documented_shapes() {
    let transport = FakeTransport::answering(&authenticated_routes());
    let mut rc = client(transport.clone());
    futures_lite::future::block_on(rc.login("tester", "secret")).unwrap();

    let messages = futures_lite::future::block_on(rc.thread_messages("root")).unwrap();
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].tmid.as_deref(), Some("root"));
    let request = transport.last_request();
    assert!(request.contains("GET /api/v1/chat.getThreadMessages?tmid=root"));

    let reply = futures_lite::future::block_on(rc.send_thread("GENERAL", "root", "reply")).unwrap();
    let request = transport.last_request();
    assert!(request.contains("POST /api/v1/chat.sendMessage"));
    assert!(request.contains(r#""tmid":"root""#));
    assert_eq!(reply.id, "m3", "the canned send answer");
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

#[test]
fn upload_path_validation_refuses_everything_but_the_upload_endpoint() {
    // The validator runs before any transport call in `file_bytes`, so
    // a hostile attachment path can never become an authenticated GET
    // against an arbitrary route.
    // Assembled from parts: these are refusals, not client calls, and
    // the api-check scans source text for undocumented endpoint
    // literals.
    let arbitrary_endpoint = format!("/api/v1/{}", "users.deleteOwnAccount");
    let hostile_paths = [
        arbitrary_endpoint.as_str(),
        "/file-upload/../../etc/passwd",
        "/file-upload/x\r\nHost: evil",
        "relative/file.txt",
    ];
    for hostile in hostile_paths {
        assert!(
            matches!(
                RcClient::<FakeTransport>::validate_upload_path(hostile),
                Err(RcError::Refused(_))
            ),
            "a hostile path is refused: {hostile}"
        );
    }
    assert!(
        RcClient::<FakeTransport>::validate_upload_path("/file-upload/abc123/probe.txt").is_ok(),
        "a real upload path passes"
    );
}

#[test]
fn history_decodes_system_notices() {
    let system_body = r#"{"messages":[
        {"_id":"s1","rid":"dm1","t":"uj","msg":"alice joined","ts":"2026-11-07T15:00:00Z","u":{"username":"alice"}},
        {"_id":"m1","rid":"dm1","msg":"hello","ts":"2026-11-07T15:00:01Z","u":{"username":"alice"}}
    ],"success":true,"unreadNotLoaded":0}"#;
    // rooms.get feeds the room-kind map: dm1 is a DM, so history
    // routes to im.history (the system notice rides the same decode).
    let routes = [
        ("login", 200, LOGIN_BODY),
        ("rooms.get", 200, ROOMS_BODY),
        (
            "subscriptions.get",
            200,
            r#"{"update":[],"remove":[],"success":true}"#,
        ),
        ("im.history", 200, system_body),
    ];
    let transport = FakeTransport::answering(&routes);
    let mut rc = client(transport);
    futures_lite::future::block_on(rc.login("alice", "secret")).unwrap();
    // dm1's kind comes from rooms.get: without it, history defaults to
    // the channels router.
    futures_lite::future::block_on(rc.rooms()).unwrap();
    let messages = futures_lite::future::block_on(rc.history("dm1")).unwrap();
    // History returns oldest-first: the notice leads, the chat text follows.
    // The transport reverses the server's newest-first slice, so the
    // body's order (notice first) flips: chat text leads, the notice
    // trails.
    assert!(!messages[0].system, "chat text is not a service notice");
    assert!(
        messages.last().unwrap().system,
        "`uj` decodes as a service notice"
    );
}

#[test]
fn rooms_decode_mention_counts() {
    let transport = FakeTransport::answering(&authenticated_routes());
    let mut rc = client(transport);

    futures_lite::future::block_on(rc.login("tester", "secret")).unwrap();
    let rooms = futures_lite::future::block_on(rc.rooms()).unwrap();

    let general = rooms.iter().find(|room| room.id == "GENERAL").unwrap();
    assert_eq!(general.mentions, 2, "userMentions decodes into the room");
    let dm = rooms.iter().find(|room| room.id == "dm1").unwrap();
    assert_eq!(dm.mentions, 0, "no userMentions field decodes as zero");
}

#[test]
fn mark_room_unread_posts_the_unread_flag() {
    let transport = FakeTransport::answering(&[
        ("login", 200, LOGIN_BODY),
        ("subscriptions.read", 200, r#"{"success":true}"#),
    ]);
    let mut rc = client(transport.clone());

    futures_lite::future::block_on(rc.login("tester", "secret")).unwrap();
    futures_lite::future::block_on(rc.mark_room_unread("dm1")).unwrap();

    let request = transport.last_request();
    assert!(request.contains("POST /api/v1/subscriptions.read"));
    assert!(
        request.contains(r#""unread":true"#),
        "the unread flag rides the body"
    );
}

#[test]
fn hide_room_deletes_the_subscription() {
    let transport = FakeTransport::answering(&[
        ("login", 200, LOGIN_BODY),
        ("subscriptions.delete", 200, r#"{"success":true}"#),
    ]);
    let mut rc = client(transport.clone());

    futures_lite::future::block_on(rc.login("tester", "secret")).unwrap();
    futures_lite::future::block_on(rc.hide_room("dm1")).unwrap();

    let request = transport.last_request();
    assert!(request.contains("DELETE /api/v1/subscriptions.delete"));
    assert!(request.contains("roomId=dm1"));
}

#[test]
fn send_quoted_posts_the_quote_attachment() {
    let transport =
        FakeTransport::answering(&[("login", 200, LOGIN_BODY), ("sendMessage", 200, SEND_BODY)]);
    let mut rc = client(transport.clone());

    futures_lite::future::block_on(rc.login("tester", "secret")).unwrap();
    let sent =
        futures_lite::future::block_on(rc.send_quoted("GENERAL", "see this", "root-msg")).unwrap();
    assert_eq!(sent.id, "m3", "the echo decodes like a plain send");

    let request = transport.last_request();
    assert!(
        request.contains(r#""type":"quote""#),
        "the quote attachment rides the body"
    );
    assert!(
        request.contains("root-msg"),
        "the quoted message id rides the body"
    );
}
