use super::*;
use serde_json::json;
use std::{
    io::{BufRead as _, BufReader, Write as _},
    net::{SocketAddr, TcpListener},
    path::Path,
};

fn owner(root: &Path, host: &str, alias: &str) -> Owner {
    Owner::new(host.into(), root, alias.into()).unwrap()
}

#[test]
fn partitions_by_host_project_and_billing_route() {
    let root = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let first = owner(root.path(), "host", "provider/model");
    let profiles: HashSet<_> = [
        first.clone(),
        owner(root.path(), "other-host", "provider/model"),
        owner(other.path(), "host", "provider/model"),
        owner(root.path(), "host", "other/model"),
    ]
    .into_iter()
    .map(|owner| Profile::prepare(root.path(), owner).unwrap().data_home())
    .collect();
    assert_eq!(profiles.len(), 4);
    assert!(profiles.contains(&Profile::prepare(root.path(), first).unwrap().data_home()));
}

struct Exchange {
    method: &'static str,
    path: &'static str,
    status: &'static str,
    headers: &'static str,
    request: Option<Value>,
    reply: String,
}

fn json_reply(method: &'static str, path: &'static str, body: Value) -> Exchange {
    Exchange {
        method,
        path,
        status: "200 OK",
        headers: "Content-Type: application/json",
        request: None,
        reply: body.to_string(),
    }
}

fn listen(owner: Owner, exchanges: Vec<Exchange>) -> (SocketAddrV4, thread::JoinHandle<()>) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let SocketAddr::V4(address) = listener.local_addr().unwrap() else {
        unreachable!("listening on IPv4");
    };
    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        for exchange in exchanges {
            let deadline = Instant::now() + IO_TIMEOUT;
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error)
                        if error.kind() == io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("Missing {} {}: {error}", exchange.method, exchange.path),
                }
            };
            socket.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
            let mut reader = BufReader::new(&mut socket);
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            let mut parts = line.split_ascii_whitespace();
            assert_eq!(parts.next(), Some(exchange.method));
            let url = Url::parse(&format!("http://127.0.0.1{}", parts.next().unwrap())).unwrap();
            assert_eq!(url.path(), exchange.path);
            assert!(
                url.query_pairs()
                    .any(|(key, value)| key == "directory" && value == owner.directory())
            );
            let mut length = 0;
            let mut authorized = false;
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                if line.trim().is_empty() {
                    break;
                }
                if let Some((name, value)) = line.trim().split_once(':') {
                    match name.to_ascii_lowercase().as_str() {
                        "content-length" => length = value.trim().parse().unwrap(),
                        "authorization" => {
                            authorized = value.trim() == "Basic b3BlbmNvZGU6c2VjcmV0"
                        }
                        _ => {}
                    }
                }
            }
            assert!(authorized);
            if let Some(expected) = exchange.request {
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), expected);
            }
            write!(
                socket,
                "HTTP/1.1 {}\r\n{}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                exchange.status,
                exchange.headers,
                exchange.reply.len(),
                exchange.reply
            )
            .unwrap();
        }
    });
    (address, server)
}

fn startup(profile: &Profile) -> Vec<Exchange> {
    vec![
        json_reply(
            "GET",
            "/global/health",
            json!({"healthy": true, "version": VERSION}),
        ),
        json_reply(
            "GET",
            "/path",
            json!({
                "directory": profile.owner().project(),
                "state": profile.state_home().join("opencode"),
                "config": profile.config_home().join("opencode"),
            }),
        ),
        json_reply(
            "POST",
            "/session",
            json!({
                "directory": profile.owner().project(), "id": "ses_owned",
            }),
        ),
    ]
}

#[test]
fn scopes_actions_history_and_stream_to_a_registered_session() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("project #1");
    std::fs::create_dir(&project).unwrap();
    let first = owner(&project, "host", "provider/model");
    let foreign = owner(&project, "other-host", "provider/model");
    let profile = Profile::prepare(tmp.path(), first.clone()).unwrap();
    let mut script = startup(&profile);
    let mut prompt = json_reply("POST", "/session/ses_owned/prompt_async", Value::Null);
    prompt.status = "204 No Content";
    prompt.reply.clear();
    prompt.request = Some(json!({
        "messageID": "msg_1", "model": {"providerID": "ox", "modelID": "provider/model"},
        "parts": [{"type": "text", "text": "hello"}],
    }));
    script.push(prompt);
    let mut history = json_reply(
        "GET",
        "/session/ses_owned/message",
        json!([{
            "info": {"sessionID": "ses_owned", "id": "msg_1"},
            "parts": [{"sessionID": "ses_owned", "messageID": "msg_1", "text": "hello"}],
        }]),
    );
    history.headers = "Content-Type: application/json\r\nX-Next-Cursor: previous";
    script.push(history);
    script.push(Exchange {
        method: "GET", path: "/event", status: "200 OK", headers: "Content-Type: text/event-stream", request: None,
        reply: concat!(
            "data: {\"id\":\"evt_connected\",\"type\":\"server.connected\",\"properties\":{}}\n\n",
            "data: {\"id\":\"evt_other\",\"type\":\"message.part.delta\",\"properties\":{\"sessionID\":\"ses_other\"}}\n\n",
            "data: {\"id\":\"evt_own\",\"type\":\"message.part.delta\",\"properties\":{\"sessionID\":\"ses_owned\"}}\n\n",
        ).into(),
    });
    let (address, server) = listen(first.clone(), script);
    let adapter = Adapter::connect(profile, address, "secret".into()).unwrap();
    let session = adapter.create(&first, "test").unwrap();
    assert!(matches!(
        adapter.history(&foreign, &session, 20, None),
        Err(Error::OwnerMismatch)
    ));
    assert!(matches!(
        adapter.history(&first, &session, 0, None),
        Err(Error::InvalidInput(_))
    ));
    adapter
        .push_text(&first, &session, "msg_1", "hello")
        .unwrap();
    let page = adapter.history(&first, &session, 1, None).unwrap();
    assert_eq!(page.before.as_deref(), Some("previous"));
    assert_eq!(page.entries[0].parts[0]["text"], "hello");
    let mut stream = adapter.events(&first, &session).unwrap();
    assert_eq!(
        stream.read_event().unwrap().unwrap().name,
        "server.connected"
    );
    assert_eq!(stream.read_event().unwrap().unwrap().id, "evt_own");
    assert!(stream.read_event().unwrap().is_none());
    server.join().unwrap();
}

#[test]
fn requires_the_owning_child_profile() {
    let tmp = tempfile::tempdir().unwrap();
    let other_state = tempfile::tempdir().unwrap();
    let scope = owner(tmp.path(), "host", "provider/model");
    let intended = Profile::prepare(tmp.path(), scope.clone()).unwrap();
    let foreign = Profile::prepare(other_state.path(), scope.clone()).unwrap();
    let mut script = startup(&foreign);
    script.pop();
    let (address, server) = listen(scope, script);
    assert!(matches!(
        Adapter::connect(intended, address, "secret".into()),
        Err(Error::OwnerMismatch)
    ));
    server.join().unwrap();
}

#[test]
fn fences_an_ambiguous_stop_until_the_exact_stale_ask_is_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let scope = owner(tmp.path(), "host", "provider/model");
    let profile = Profile::prepare(tmp.path(), scope.clone()).unwrap();
    let mut script = startup(&profile);
    let owned = json!({"id": "per_owned", "sessionID": "ses_owned", "permission": "bash", "patterns": ["fixture"]});
    let foreign = json!({"id": "per_other", "sessionID": "ses_other", "permission": "bash", "patterns": ["fixture"]});
    let mut failed_abort = json_reply("POST", "/session/ses_owned/abort", Value::Null);
    failed_abort.status = "500 Internal Server Error";
    script.extend([
        failed_abort,
        json_reply("POST", "/session/ses_owned/abort", json!(true)),
        json_reply("GET", "/session/status", json!({})),
        json_reply("GET", "/permission", json!([owned, foreign])),
        Exchange {
            request: Some(json!({"reply": "reject"})),
            ..json_reply("POST", "/permission/per_owned/reply", json!(true))
        },
        json_reply("GET", "/permission", json!([foreign])),
        json_reply("GET", "/permission", json!([foreign])),
    ]);
    let (address, server) = listen(scope.clone(), script);
    let mut adapter = Adapter::connect(profile, address, "secret".into()).unwrap();
    let session = adapter.create(&scope, "test").unwrap();
    assert!(matches!(
        adapter.cancel(&scope, &session),
        Err(Error::NativeStatus(StatusCode::INTERNAL_SERVER_ERROR))
    ));
    assert!(matches!(
        adapter.push_text(&scope, &session, "msg_1", "hello"),
        Err(Error::BlockedByCancellation)
    ));
    assert!(matches!(
        adapter.answer(&scope, &session, "per_owned", Decision::AllowOnce),
        Err(Error::BlockedByCancellation)
    ));
    adapter.cancel(&scope, &session).unwrap();
    assert!(matches!(
        adapter.answer(&scope, &session, "per_other", Decision::AllowOnce),
        Err(Error::UnknownPermission)
    ));
    server.join().unwrap();
}
