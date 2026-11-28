use crate::{bridge, server::App, state::now};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD as B64};
use ox_desk_core::host_protocol::{Client, Cursor, NotificationKind, Scope, Server, VERSION};
use p256::{
    PublicKey, SecretKey,
    ecdsa::{Signature, SigningKey, signature::Signer},
    elliptic_curve::sec1::ToEncodedPoint,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, VecDeque},
    io,
    net::{IpAddr, SocketAddr},
    sync::Arc,
    time::{Duration, SystemTime},
};
use tokio::{
    io::BufReader,
    sync::{mpsc, watch},
    time::Instant,
};
use url::Url;
use web_push_native::{Auth, WebPushBuilder};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct Preferences {
    enabled: bool,
    approval: bool,
    question: bool,
    finished: bool,
    goal_reached: bool,
    failed: bool,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            enabled: true,
            approval: true,
            question: true,
            finished: true,
            goal_reached: false,
            failed: true,
        }
    }
}
impl Preferences {
    pub(crate) fn is_default(&self) -> bool {
        *self == Self::default()
    }

    pub(crate) fn set(&mut self, change: PreferenceEdit) {
        let value = match change.event {
            Preference::Enabled => &mut self.enabled,
            Preference::Approval => &mut self.approval,
            Preference::Question => &mut self.question,
            Preference::Finished => &mut self.finished,
            Preference::GoalReached => &mut self.goal_reached,
            Preference::Failed => &mut self.failed,
        };
        *value = change.value;
    }

    pub(crate) fn allows(&self, event: Event) -> bool {
        self.enabled
            && match event {
                Event::Session(NotificationKind::Approval) => self.approval,
                Event::Session(NotificationKind::Question) => self.question,
                Event::Session(NotificationKind::Finished) => self.finished,
                Event::Session(NotificationKind::Failed) => self.failed,
                Event::GoalReached => self.goal_reached,
            }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Event {
    Session(NotificationKind),
    GoalReached,
}
impl Serialize for Event {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Session(kind) => kind.serialize(serializer),
            Self::GoalReached => serializer.serialize_str("goal_reached"),
        }
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Preference {
    Enabled,
    Approval,
    Question,
    Finished,
    GoalReached,
    Failed,
}

#[derive(Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreferenceEdit {
    pub event: Preference,
    pub value: bool,
}

#[derive(Serialize)]
pub(crate) struct PreferenceState {
    pub preferences: Preferences,
    pub subscribed: bool,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subscription {
    pub endpoint: String,
    pub keys: Keys,
}
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keys {
    pub p256dh: String,
    pub auth: String,
}
impl Subscription {
    fn url(&self) -> Option<Url> {
        if self.endpoint.len() > 4096 {
            return None;
        }
        let u = Url::parse(&self.endpoint).ok()?;
        let path = u.path();
        let allowed = match u.host_str()? {
            "fcm.googleapis.com" => path
                .strip_prefix("/fcm/send/")
                .or_else(|| path.strip_prefix("/wp/")),
            "updates.push.services.mozilla.com" => path
                .strip_prefix("/wpush/v2/")
                .or_else(|| path.strip_prefix("/wpush/v1/")),
            "web.push.apple.com" => path.strip_prefix('/'),
            _ => None,
        }?;
        (u.scheme() == "https"
            && u.port().is_none()
            && u.username().is_empty()
            && u.password().is_none()
            && u.fragment().is_none()
            && u.query().is_none()
            && u.as_str() == self.endpoint
            && !allowed.is_empty()
            && allowed
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_:=".contains(&b)))
        .then_some(u)
    }
    fn keys(&self) -> Option<(PublicKey, [u8; 16])> {
        let key = B64.decode(&self.keys.p256dh).ok()?;
        if key.len() != 65 {
            return None;
        }
        Some((
            PublicKey::from_sec1_bytes(&key).ok()?,
            B64.decode(&self.keys.auth).ok()?.try_into().ok()?,
        ))
    }
    pub fn valid(&self) -> bool {
        self.url().is_some() && self.keys().is_some()
    }
}
#[derive(Serialize)]
struct Payload<'a> {
    thread: &'a str,
    title: &'a str,
    kind: Event,
}
fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => {
            let [x, y, z, _] = a.octets();
            !(x == 0
                || x == 10
                || x == 127
                || x >= 224
                || x == 169 && y == 254
                || x == 172 && (16..32).contains(&y)
                || x == 192 && (y == 168 || y == 0 || y == 2 || y == 88 && z == 99)
                || x == 100 && (64..128).contains(&y)
                || x == 198 && (y == 18 || y == 19 || y == 51 && z == 100)
                || x == 203 && y == 0 && z == 113)
        }
        IpAddr::V6(a) => {
            let s = a.segments();
            s[0] & 0xe000 == 0x2000
                && !(s[0] == 0x2001 && (s[1] <= 0x1ff || s[1] == 0xdb8) || s[0] == 0x2002)
        }
    }
}
fn encrypt(
    sub: &Subscription,
    key: &SecretKey,
    data: &[u8],
    contact: &str,
    ttl: Duration,
) -> Option<(Url, reqwest::header::HeaderMap, Vec<u8>)> {
    let url = sub.url()?;
    let (public, auth) = sub.keys()?;
    let encrypted = WebPushBuilder::new(url.as_str().parse().ok()?, public, Auth::from(auth))
        .with_valid_duration(ttl)
        .build(data)
        .ok()?;
    let (parts, body) = encrypted.into_parts();
    let claims=B64.encode(serde_json::to_vec(&serde_json::json!({"aud":url.origin().ascii_serialization(),"exp":now()+3600,"sub":contact})).ok()?);
    let signing = format!(
        "{}.{}",
        B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#),
        claims
    );
    let signature: Signature = SigningKey::from(key).sign(signing.as_bytes());
    let auth = format!(
        "vapid t={signing}.{}, k={}",
        B64.encode(signature.to_bytes()),
        B64.encode(key.public_key().to_encoded_point(false).as_bytes())
    );
    let mut headers = parts.headers;
    headers.insert("authorization", auth.parse().ok()?);
    Some((url, headers, body))
}
#[derive(Debug, PartialEq, Eq)]
enum DeliveryResult {
    Done,
    Gone,
    Retry(Duration),
}

fn response(status: u16, retry_after: Option<&str>, now: SystemTime) -> DeliveryResult {
    match status {
        404 | 410 => DeliveryResult::Gone,
        408 | 429 | 500..=599 => {
            let delay = retry_after
                .and_then(|value| {
                    value
                        .parse::<u64>()
                        .ok()
                        .map(Duration::from_secs)
                        .or_else(|| {
                            httpdate::parse_http_date(value)
                                .ok()
                                .map(|date| date.duration_since(now).unwrap_or_default())
                        })
                })
                .unwrap_or_default();
            DeliveryResult::Retry(delay)
        }
        _ => DeliveryResult::Done,
    }
}

async fn send(
    sub: &Subscription,
    key: &SecretKey,
    data: &[u8],
    contact: &str,
    ttl: Duration,
) -> DeliveryResult {
    let Some((url, headers, body)) = encrypt(sub, key, data, contact, ttl) else {
        return DeliveryResult::Done;
    };
    let host = url.host_str().expect("validated subscription host");
    let Ok(resolved) = tokio::net::lookup_host((host, 443)).await else {
        return DeliveryResult::Retry(Duration::ZERO);
    };
    let addrs: Vec<SocketAddr> = resolved.collect();
    if addrs.is_empty() {
        return DeliveryResult::Retry(Duration::ZERO);
    }
    if addrs.iter().any(|addr| !public_ip(addr.ip())) {
        return DeliveryResult::Done;
    }
    let Ok(client) = client_builder().resolve_to_addrs(host, &addrs).build() else {
        return DeliveryResult::Done;
    };
    post(client, url, headers, body).await
}
fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(5))
}
async fn post(
    client: reqwest::Client,
    url: Url,
    headers: reqwest::header::HeaderMap,
    body: Vec<u8>,
) -> DeliveryResult {
    let Ok(res) = client.post(url).headers(headers).body(body).send().await else {
        return DeliveryResult::Retry(Duration::ZERO);
    };
    response(
        res.status().as_u16(),
        res.headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|h| h.to_str().ok()),
        SystemTime::now(),
    )
}

struct Notice {
    bytes: Vec<u8>,
    goal_bytes: Option<Vec<u8>>,
    expires: Instant,
    event: Event,
    // Closing the sender cancels every device's queued or in-flight copy.
    ask: Option<watch::Receiver<()>>,
}

struct Delivery {
    device: String,
    subscription: Subscription,
    notice: Arc<Notice>,
    event: Event,
    attempts: u32,
}

impl Delivery {
    async fn attempt(
        &self,
        app: &App,
        transport: &impl AsyncFn(&Subscription, &SecretKey, &[u8], &str, Duration) -> DeliveryResult,
    ) -> DeliveryResult {
        let mut changed = app.state.lock().unwrap().changed.subscribe();
        let mut ask = self.notice.ask.clone();
        let (key, config) = {
            let mut state = app.state.lock().unwrap();
            (state.vapid(), state.configuration())
        };
        let (Ok(key), Some((contact, _))) = (key, config) else {
            return DeliveryResult::Done;
        };
        let ttl = self
            .notice
            .expires
            .saturating_duration_since(Instant::now());
        if ttl.is_zero() {
            return DeliveryResult::Done;
        }
        let payload = if self.event == Event::GoalReached {
            self.notice
                .goal_bytes
                .as_ref()
                .expect("selected goal payload")
        } else {
            &self.notice.bytes
        };
        let post = tokio::time::timeout(
            Duration::from_secs(7).min(ttl),
            transport(&self.subscription, &key, payload, &contact, ttl),
        );
        tokio::pin!(post);
        loop {
            {
                let state = app.state.lock().unwrap();
                if !state.is_subscribed(&self.device, &self.subscription)
                    || !state.allows_push(&self.device, self.event)
                {
                    return DeliveryResult::Done;
                }
            }
            tokio::select! {biased;
                _ = async {
                    if let Some(rx) = &mut ask { let _ = rx.changed().await; }
                }, if self.notice.ask.is_some() => return DeliveryResult::Done,
                _ = changed.changed() => continue,
                result = &mut post => return result.unwrap_or(DeliveryResult::Retry(Duration::ZERO)),
            }
        }
    }

    fn retry_at(&self, minimum: Duration) -> Option<Instant> {
        if self.attempts >= 4 {
            return None;
        }
        let backoff = Duration::from_secs(2u64.pow(self.attempts));
        Instant::now().checked_add(minimum.max(backoff))
    }
}

// Keep delayed retries separate so a push service asking us to wait never
// sleeps the sender for the next phone or session. All payloads stay in memory.
async fn deliveries(
    app: App,
    mut rx: mpsc::Receiver<Notice>,
    transport: impl AsyncFn(&Subscription, &SecretKey, &[u8], &str, Duration) -> DeliveryResult,
) {
    const MAX_RETRIES_QUEUED: usize = 256;
    let mut fresh = VecDeque::new();
    let mut retries = BTreeMap::<(Instant, u64), Delivery>::new();
    let mut cooldowns = HashMap::<String, (Instant, Duration)>::new();
    let mut sequence = 0u64;
    loop {
        let next = retries.first_key_value().map(|((at, _), _)| *at);
        let mut job = if let Some(job) = fresh.pop_front() {
            job
        } else if next.is_some_and(|at| at <= Instant::now()) {
            retries.pop_first().unwrap().1
        } else {
            tokio::select! {
                notice = rx.recv() => {
                    let Some(notice) = notice else { return };
                    let notice = Arc::new(notice);
                    let state = app.state.lock().unwrap();
                    let subscribers = state.subscriptions();
                    cooldowns.retain(|endpoint, (since, delay)| since.elapsed() < *delay
                        && subscribers.iter().any(|(_, sub)| sub.endpoint == *endpoint));
                    for (device, subscription) in subscribers {
                        let event = if notice.goal_bytes.is_some() && state.allows_push(&device, Event::GoalReached) {
                            Event::GoalReached
                        } else if state.allows_push(&device, notice.event) {
                            notice.event
                        } else {
                            continue;
                        };
                        fresh.push_back(Delivery { device, subscription, notice: notice.clone(), event, attempts: 0 });
                    }
                }
                _ = async { tokio::time::sleep_until(next.unwrap()).await }, if next.is_some() => {}
            }
            continue;
        };
        let cooldown = cooldowns
            .get(&job.subscription.endpoint)
            .map(|(since, delay)| delay.saturating_sub(since.elapsed()))
            .unwrap_or_default();
        let next_attempt = if cooldown.is_zero() {
            job.attempts += 1;
            match job.attempt(&app, &transport).await {
                DeliveryResult::Gone => {
                    let mut state = app.state.lock().unwrap();
                    if state.is_subscribed(&job.device, &job.subscription) {
                        let _ = state.set_push(&job.device, None);
                    }
                    None
                }
                DeliveryResult::Retry(delay) => {
                    if !delay.is_zero() {
                        cooldowns
                            .insert(job.subscription.endpoint.clone(), (Instant::now(), delay));
                    }
                    job.retry_at(delay)
                }
                DeliveryResult::Done => None,
            }
        } else {
            Instant::now().checked_add(cooldown)
        };
        if let Some(at) = next_attempt
            && at < job.notice.expires
            && retries.len() < MAX_RETRIES_QUEUED
        {
            sequence = sequence.wrapping_add(1);
            retries.insert((at, sequence), job);
        }
    }
}

pub(crate) async fn watch(app: App) {
    let mut wake = app.state.lock().unwrap().changed.subscribe();
    let (tx, rx) = mpsc::channel(32);
    tokio::spawn(deliveries(app.clone(), rx, send));
    let mut cursor = None;
    let mut asks = ActiveAsks::default();
    loop {
        if app.state.lock().unwrap().subscriptions().is_empty() {
            cursor = None;
            asks.0.clear();
            let _ = wake.changed().await;
            continue;
        }
        {
            let feed = notifications(&app, &mut cursor, &tx, &mut asks);
            tokio::pin!(feed);
            loop {
                tokio::select! {
                    _ = &mut feed => break,
                    _ = wake.changed() => {
                        if app.state.lock().unwrap().subscriptions().is_empty() { break; }
                    }
                }
            }
        }
        // While disconnected we cannot know whether the user answered. Cancel
        // pending sends, retaining the known tokens to identify new asks on replay.
        for scope in asks.0.values_mut() {
            for sender in scope.values_mut() {
                *sender = watch::channel(()).0;
            }
        }
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(3)) => {},
            _ = wake.changed() => {},
        }
    }
}
async fn notifications(
    app: &App,
    cursor: &mut Option<Cursor>,
    tx: &mpsc::Sender<Notice>,
    asks: &mut ActiveAsks,
) -> io::Result<()> {
    let host = bridge::connect(&app.host).await?;
    let (r, mut w) = host.into_split();
    let mut r = BufReader::new(r);
    let mut buf = Vec::new();
    bridge::write(&mut w, &Client::Hello { version: VERSION }).await?;
    if !matches!(
        serde_json::from_slice::<Server>(&bridge::line(&mut r, &mut buf).await?)?,
        Server::Hello { version, .. } if ox_desk_core::host_protocol::supported(version)
    ) {
        return Err(io::Error::other("Host version mismatch"));
    }
    bridge::write(
        &mut w,
        &Client::Subscribe {
            scope: None,
            after: cursor.clone(),
            terminal: false,
        },
    )
    .await?;
    loop {
        let bytes = bridge::line(&mut r, &mut buf).await?;
        match notices(&bytes, cursor.as_ref(), asks)? {
            Notices::Skip => {}
            Notices::Snapshot(c) => *cursor = Some(c),
            Notices::Delta(c, payloads) => {
                for data in payloads {
                    if tx.send(data).await.is_err() {
                        return Ok(());
                    }
                }
                *cursor = Some(c);
            }
        }
    }
}
enum Notices {
    Skip,
    Snapshot(Cursor),
    Delta(Cursor, Vec<Notice>),
}
// The host gives each live ask a unique token and includes the updated asks in
// the notification frame. Keep only a digest of that token. A new ask on the
// same thread must never make a retry for its predecessor valid again.
#[derive(Default)]
struct ActiveAsks(HashMap<Scope, HashMap<[u8; 32], watch::Sender<()>>>);

struct NewAsk {
    thread: String,
    kind: NotificationKind,
    digest: [u8; 32],
    done: watch::Receiver<()>,
}

impl ActiveAsks {
    fn replace(&mut self, scope: &Scope, asks: Vec<NoticeAsk>) -> Vec<NewAsk> {
        let mut previous = self.0.remove(scope).unwrap_or_default();
        let mut current = HashMap::new();
        let mut added = Vec::new();
        for ask in asks {
            let Some(thread) = ask.thread.or_else(|| {
                let id = ask.id.as_ref()?.as_str()?;
                id.starts_with("ox-yolo-").then(|| format!("ask:{id}"))
            }) else {
                continue;
            };
            let token: [u8; 32] = ring::digest::digest(&ring::digest::SHA256, ask.token.as_bytes())
                .as_ref()
                .try_into()
                .unwrap();
            let sender = previous.remove(&token).unwrap_or_else(|| {
                let (tx, rx) = watch::channel(());
                let kind = if ask.questions.is_empty() {
                    NotificationKind::Approval
                } else {
                    NotificationKind::Question
                };
                added.push(NewAsk {
                    thread,
                    kind,
                    digest: token,
                    done: rx,
                });
                tx
            });
            current.insert(token, sender);
        }
        if !current.is_empty() {
            self.0.insert(scope.clone(), current);
        }
        added
    }
}
/// Turn one host frame into Web Push payloads. Each payload holds only the scoped
/// thread identifier, the session title and the event kind.
fn notices(bytes: &[u8], cursor: Option<&Cursor>, asks: &mut ActiveAsks) -> io::Result<Notices> {
    let frame: NoticeFrame = serde_json::from_slice(bytes)?;
    let Some(c) = frame.cursor else {
        if frame.kind == "error" {
            return Err(io::Error::other("Host subscription closed"));
        }
        return Ok(Notices::Skip);
    };
    if frame.kind == "snapshot" {
        asks.0.clear();
        for state in frame.scopes {
            asks.replace(&state.scope, state.asks);
        }
        return Ok(Notices::Snapshot(c));
    }
    if frame.kind != "delta" || cursor.is_some_and(|old| old.epoch == c.epoch && old.seq >= c.seq) {
        return Ok(Notices::Skip);
    }
    let Some(scope) = frame.scope else {
        return Ok(Notices::Skip);
    };
    let mut changes = frame.changes;
    let mut new_asks = changes
        .iter_mut()
        .rev()
        .find(|change| change.kind == "asks")
        .map(|change| asks.replace(&scope, std::mem::take(&mut change.asks)))
        .unwrap_or_default();
    let mut notifications: Vec<_> = changes
        .into_iter()
        .filter(|change| change.kind == "notification")
        .filter_map(|c| Some((c.thread?, c.title?, c.event?, c.goal_reached)))
        .collect();
    // A draft has no native thread yet. Address its live ask so a stale tap
    // cannot select a later draft on the same project and route.
    notifications.extend(
        new_asks
            .iter()
            .filter(|ask| ask.thread.starts_with("ask:ox-yolo-"))
            .map(|ask| (ask.thread.clone(), "New OX session".into(), ask.kind, false)),
    );
    let mut payloads = Vec::new();
    for (thread, title, kind, goal_reached) in notifications {
        let ask = if matches!(
            kind,
            NotificationKind::Approval | NotificationKind::Question
        ) {
            let Some(index) = new_asks
                .iter()
                .position(|ask| ask.thread == thread && ask.kind == kind)
            else {
                continue;
            };
            Some(new_asks.remove(index))
        } else {
            None
        };
        let ttl = Duration::from_secs(if ask.is_some() { 300 } else { 3600 });
        let title = title
            .chars()
            .filter(|c| !c.is_control())
            .take(80)
            .collect::<String>();
        let mut thread = crate::phone_link::session_target(&scope, &thread)?;
        if let Some(ask) = &ask {
            thread.push_str("~ask=");
            thread.push_str(&B64.encode(ask.digest));
        }
        let event = Event::Session(kind);
        let payload = |kind| {
            serde_json::to_vec(&Payload {
                thread: &thread,
                title: &title,
                kind,
            })
        };
        payloads.push(Notice {
            ask: ask.map(|ask| ask.done),
            expires: Instant::now() + ttl,
            event,
            bytes: payload(event)?,
            goal_bytes: (goal_reached && kind == NotificationKind::Finished)
                .then(|| payload(Event::GoalReached))
                .transpose()?,
        });
    }
    Ok(Notices::Delta(c, payloads))
}

// Push never materializes transcript bodies or tool output from a global subscription.
#[derive(Deserialize)]
struct NoticeFrame {
    #[serde(rename = "type")]
    kind: String,
    cursor: Option<Cursor>,
    scope: Option<Scope>,
    #[serde(default)]
    changes: Vec<NoticeChange>,
    #[serde(default)]
    scopes: Vec<NoticeScope>,
}
#[derive(Deserialize)]
struct NoticeScope {
    scope: Scope,
    #[serde(default)]
    asks: Vec<NoticeAsk>,
}
#[derive(Deserialize)]
struct NoticeAsk {
    id: Option<serde_json::Value>,
    thread: Option<String>,
    token: String,
    #[serde(default)]
    questions: Vec<serde::de::IgnoredAny>,
}
#[derive(Deserialize)]
struct NoticeChange {
    kind: String,
    thread: Option<String>,
    title: Option<String>,
    event: Option<NotificationKind>,
    #[serde(default)]
    goal_reached: bool,
    #[serde(default)]
    asks: Vec<NoticeAsk>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::{VerifyingKey, signature::Verifier};
    use p256::elliptic_curve::rand_core::OsRng;
    use serde_json::json;
    pub(super) fn fixture() -> (Subscription, SecretKey) {
        let phone = SecretKey::random(&mut OsRng);
        (
            Subscription {
                endpoint: "https://updates.push.services.mozilla.com/wpush/v2/fixture".into(),
                keys: Keys {
                    p256dh: B64.encode(phone.public_key().to_encoded_point(false).as_bytes()),
                    auth: B64.encode([7; 16]),
                },
            },
            phone,
        )
    }
    #[test]
    fn encrypted_push_has_only_metadata_and_origin_scoped_vapid_signature() {
        let (sub, phone) = fixture();
        let vapid = SecretKey::random(&mut OsRng);
        let payload = br#"{"thread":"one","title":"OX session","kind":"approval"}"#;
        let (_, headers, body) = encrypt(
            &sub,
            &vapid,
            payload,
            "https://test.tail.ts.net",
            Duration::from_secs(300),
        )
        .unwrap();
        assert!(!body.windows(7).any(|p| p == b"session"));
        let clear = web_push_native::decrypt(body, &phone, &Auth::from([7; 16])).unwrap();
        assert_eq!(clear, payload);
        let (token, key) = headers["authorization"]
            .to_str()
            .unwrap()
            .strip_prefix("vapid t=")
            .unwrap()
            .split_once(", k=")
            .unwrap();
        let (signed, sig) = token.rsplit_once('.').unwrap();
        let signature = Signature::from_slice(&B64.decode(sig).unwrap()).unwrap();
        VerifyingKey::from_sec1_bytes(&B64.decode(key).unwrap())
            .unwrap()
            .verify(signed.as_bytes(), &signature)
            .unwrap();
        let claims: serde_json::Value =
            serde_json::from_slice(&B64.decode(signed.split_once('.').unwrap().1).unwrap())
                .unwrap();
        assert_eq!(claims["aud"], "https://updates.push.services.mozilla.com");
        assert!(claims["exp"].as_i64().unwrap() > now());
    }
    #[tokio::test]
    async fn push_delivers_an_encrypted_post_to_a_tls_service() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};
        let dir = tempfile::tempdir().unwrap();
        let mut server = Command::new("python3")
            .arg(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/push_tls.py"))
            .arg(dir.path())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut port = String::new();
        BufReader::new(server.stdout.take().unwrap())
            .read_line(&mut port)
            .unwrap();
        let address = format!("127.0.0.1:{}", port.trim()).parse().unwrap();
        let (mut sub, phone) = fixture();
        sub.endpoint = "https://web.push.apple.com/synthetic".into();
        let (url, headers, body) = encrypt(
            &sub,
            &SecretKey::random(&mut OsRng),
            b"fixture",
            "https://host.fixture.ts.net",
            Duration::from_secs(300),
        )
        .unwrap();
        let cert =
            reqwest::Certificate::from_pem(&std::fs::read(dir.path().join("ca.pem")).unwrap())
                .unwrap();
        let client = client_builder()
            .add_root_certificate(cert)
            .resolve("web.push.apple.com", address)
            .build()
            .unwrap();
        assert_eq!(post(client, url, headers, body).await, DeliveryResult::Done);
        assert!(server.wait().unwrap().success());
        let ciphertext = std::fs::read(dir.path().join("encrypted")).unwrap();
        assert_eq!(
            web_push_native::decrypt(ciphertext, &phone, &Auth::from([7; 16])).unwrap(),
            b"fixture"
        );
    }
    #[test]
    fn push_payloads_carry_only_scoped_thread_title_and_kind() {
        use ox_desk_core::{
            asks::{CommandApproval, Question},
            host_protocol::{AskView, Change, SessionView, Timeline, TimelineItem},
            runtime::{Billing, RequestId, Route, Runtime},
        };
        const SECRET: &str = "SECRET-7f3a";
        let leak = |what: &str| format!("{SECRET}-{what}");
        // Every kind must be listed here; adding one fails this match until it is covered.
        let all = [
            NotificationKind::Finished,
            NotificationKind::Failed,
            NotificationKind::Approval,
            NotificationKind::Question,
        ];
        for kind in all {
            match kind {
                NotificationKind::Finished
                | NotificationKind::Failed
                | NotificationKind::Approval
                | NotificationKind::Question => {}
            }
        }
        for (n, kind) in all.into_iter().enumerate() {
            let scope = Scope {
                project: format!("/home/me/{}", leak("project")).into(),
                route: Route {
                    runtime: Runtime::Claude,
                    billing: Billing::Api(leak("alias")),
                },
            };
            let question = Question {
                id: "q".into(),
                question: leak("question"),
                header: leak("header"),
                is_other: false,
                is_secret: true,
                multi_select: false,
                options: None,
            };
            let ask = AskView {
                command: Some(CommandApproval {
                    command: format!("curl -H 'Authorization: {}' https://x", leak("command")),
                    cwd: Some(format!("/srv/{}", leak("cwd")).into()),
                    decline: 1,
                }),
                id: RequestId::Text(leak("request")),
                token: leak("token"),
                thread: Some("thread-1".into()),
                turn: Some("turn".into()),
                item: Some("item".into()),
                choice_details: vec![Some(leak("choice-detail"))],
                title: format!("Run {}?", leak("ask-title")),
                detail: format!("--- a/x\n+++ b/x\n+{}", leak("diff")),
                choices: vec![leak("choice")],
                questions: if kind == NotificationKind::Question {
                    vec![question]
                } else {
                    vec![]
                },
            };
            let item = TimelineItem {
                turn: "turn".into(),
                id: "item".into(),
                role: "tool".into(),
                text: leak("message-body"),
                revision: 1,
                detail: json!({"arguments": {"path": leak("tool-argument")}, "diff": leak("diff")}),
            };
            let frame = Server::Delta {
                cursor: Cursor {
                    epoch: "e".into(),
                    seq: 1 + n as u64,
                },
                scope: scope.clone(),
                changes: vec![
                    Change::Asks { asks: vec![ask] },
                    Change::Session {
                        session: SessionView {
                            id: "thread-1".into(),
                            title: "Fix login".into(),
                            parent: None,
                            status: "running".into(),
                            updated_at: 0,
                            capabilities: vec![],
                            detail: json!({"last_error": leak("error")}),
                        },
                    },
                    Change::Transcript {
                        thread: "thread-1".into(),
                        timeline: Timeline {
                            items: vec![item.clone()],
                            detail: json!({"pending": leak("timeline")}),
                        },
                    },
                    Change::Items {
                        thread: "thread-1".into(),
                        items: vec![item],
                    },
                    Change::Append {
                        thread: "thread-1".into(),
                        turn: "turn".into(),
                        id: "item".into(),
                        text: leak("append"),
                        revision: 2,
                    },
                    Change::TimelineState {
                        thread: "thread-1".into(),
                        detail: json!({"command": leak("state")}),
                    },
                    Change::Notification {
                        thread: "thread-1".into(),
                        title: "Fix login\u{7}".into(),
                        event: kind,
                        goal_reached: false,
                    },
                ],
            };
            let bytes = serde_json::to_vec(&frame).unwrap();
            assert!(String::from_utf8_lossy(&bytes).contains(SECRET));
            let Notices::Delta(_, payloads) =
                notices(&bytes, None, &mut ActiveAsks::default()).unwrap()
            else {
                panic!("delta frame was not processed");
            };
            assert_eq!(payloads.len(), 1, "{kind:?}");
            let text = String::from_utf8(payloads[0].bytes.clone()).unwrap();
            assert!(!text.contains(SECRET), "{kind:?} payload leaked: {text}");
            let v: serde_json::Value = serde_json::from_str(&text).unwrap();
            let mut keys: Vec<_> = v.as_object().unwrap().keys().cloned().collect();
            keys.sort();
            assert_eq!(keys, ["kind", "thread", "title"]);
            assert_eq!(v["kind"], serde_json::to_value(kind).unwrap());
            assert_eq!(v["title"], "Fix login");
            let thread = v["thread"].as_str().unwrap();
            let target = crate::phone_link::session_target(&scope, "thread-1").unwrap();
            if matches!(
                kind,
                NotificationKind::Approval | NotificationKind::Question
            ) {
                let digest = ring::digest::digest(&ring::digest::SHA256, leak("token").as_bytes());
                assert_eq!(thread, format!("{target}~ask={}", B64.encode(digest)));
            } else {
                assert_eq!(thread, target);
            }
        }
    }
    #[test]
    fn push_endpoint_and_destination_are_constrained() {
        let (mut sub, _) = fixture();
        assert!(sub.valid());
        for endpoint in [
            "http://127.0.0.1/push",
            "https://user:pw@web.push.apple.com/abc",
            "https://updates.push.services.mozilla.com.evil.test/wpush/v2/x",
            "https://fcm.googleapis.com/v1/projects/me/messages:send",
            "https://web.push.apple.com:8443/path",
            "https://web.push.apple.com//evil",
            "https://web.push.apple.com/?injected=true",
        ] {
            sub.endpoint = endpoint.into();
            assert!(!sub.valid());
        }
        for ip in [
            "127.0.0.1",
            "100.85.2.3",
            "192.168.1.1",
            "10.0.0.1",
            "::1",
            "fe80::1",
            "::ffff:127.0.0.1",
            "fd7a:115c:a1e0::1",
            "2002:7f00:1::",
        ] {
            assert!(!public_ip(ip.parse().unwrap()));
        }
    }
}

#[cfg(test)]
mod retry_tests;
