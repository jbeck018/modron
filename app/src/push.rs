//! Web push for the web host (Packet PUSH-2), target-neutral: the Settings
//! row's state and words, the request bodies, and the base64url codec the
//! browser's keys need. The browser calls (service worker, PushManager,
//! Notification, fetch) live in `web.rs` (wasm32 only). The bridge's routes
//! are in cloud/bridge/README.md ("Web push").

use modron_mobile_core::view::{
    push_line, PUSH_BLOCKED_LABEL, PUSH_HOME_SCREEN_HINT, PUSH_LABEL, PUSH_OFF_LABEL,
    PUSH_ON_LABEL, PUSH_UNAVAILABLE_LABEL,
};
use serde_json::json;

/// What `Notification.permission` says (`default` when the browser has none).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Permission {
    Default,
    Granted,
    Denied,
}

impl Permission {
    pub fn parse(text: &str) -> Self {
        match text {
            "granted" => Self::Granted,
            "denied" => Self::Denied,
            _ => Self::Default,
        }
    }
}

/// The role the page's push calls run as. The bridge answers `GET
/// /api/push/key` 200 to the owner and to an approver invite, 403 to a viewer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Owner,
    Approver,
    Viewer,
}

/// The role from the key route's answer and whether the page carries an
/// invite. Anything but 200 is a viewer here (the row is hidden either way).
pub fn role_of(key_status: Option<u16>, invited: bool) -> Role {
    match (key_status, invited) {
        (Some(200), false) => Role::Owner,
        (Some(200), true) => Role::Approver,
        _ => Role::Viewer,
    }
}

/// Everything the row's state depends on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Facts {
    /// The `GET /api/push/key` status; `None` when the request got no answer.
    pub key_status: Option<u16>,
    pub role: Role,
    /// `navigator.serviceWorker` and `PushManager` both exist.
    pub supported: bool,
    /// iOS outside a Home Screen web app: push works only once added there.
    pub ios_tab: bool,
    pub permission: Permission,
    /// `pushManager.getSubscription()` found one for this page's scope.
    pub subscribed: bool,
}

/// The Settings row's state. `Hidden` means no row at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowState {
    Hidden,
    Off,
    On,
    Unavailable,
}

/// The row's state. Shown only when the key route answers 200 for a non-viewer
/// and the browser supports push. Where it does not, an iOS tab shows the row
/// as unavailable (with the Home Screen hint); any other browser hides it.
/// Permission does not change the state: a blocked permission leaves the row
/// off, and [`view`] names the reason.
pub fn row_state(facts: &Facts) -> RowState {
    if facts.key_status != Some(200) || facts.role == Role::Viewer {
        return RowState::Hidden;
    }
    if facts.supported {
        if facts.subscribed {
            RowState::On
        } else {
            RowState::Off
        }
    } else if facts.ios_tab {
        RowState::Unavailable
    } else {
        RowState::Hidden
    }
}

/// What the Settings row shows. `line` is the shared words (`notify this
/// device: on`); a tap toggles only when `tappable` (off or on).
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct PushView {
    pub line: String,
    /// The row's label and value, apart (the web draws them as label and
    /// value; `line` is their join, for the terminal).
    pub label: &'static str,
    pub value: &'static str,
    /// Whether the value is on (the web's switch).
    pub on: bool,
    pub tappable: bool,
    /// The iOS Home Screen hint, under an unavailable row.
    pub hint: Option<String>,
    /// The reason for the last refused or failed tap, or that the permission
    /// is blocked (shared words).
    pub note: Option<String>,
}

/// The row for `facts`, or `None` when it is hidden. `error` is the last tap's
/// refusal or failure, which takes the note's place.
pub fn view(facts: &Facts, error: Option<&str>) -> Option<PushView> {
    let state = row_state(facts);
    let value = match state {
        RowState::Hidden => return None,
        RowState::Off => PUSH_OFF_LABEL,
        RowState::On => PUSH_ON_LABEL,
        RowState::Unavailable => PUSH_UNAVAILABLE_LABEL,
    };
    let blocked = (state == RowState::Off && facts.permission == Permission::Denied)
        .then_some(PUSH_BLOCKED_LABEL);
    Some(PushView {
        line: push_line(value),
        label: PUSH_LABEL,
        value,
        on: state == RowState::On,
        tappable: matches!(state, RowState::Off | RowState::On),
        hint: (state == RowState::Unavailable).then(|| PUSH_HOME_SCREEN_HINT.to_string()),
        note: error.or(blocked).map(str::to_string),
    })
}

/// Whether a user agent is an iPhone, iPad or iPod (iOS browsers all run
/// WebKit, so push needs the Home Screen app there).
pub fn is_ios(user_agent: &str) -> bool {
    ["iPhone", "iPad", "iPod"].iter().any(|device| user_agent.contains(device))
}

/// The `invite` parameter of the page address (`?invite=<id>.<sig>`), when it
/// is a plausible invite token. Other characters are refused, so the value
/// can be sent in a query or a body as it is.
pub fn invite_from_query(query: &str) -> Option<String> {
    query
        .trim_start_matches('?')
        .split('&')
        .find_map(|pair| pair.strip_prefix("invite="))
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 256
                && value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
        })
        .map(str::to_string)
}

const B64URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// Unpadded base64url (RFC 4648 section 5), as the Web Push keys use.
pub fn b64url_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = u32::from(chunk[0]);
        let b1 = u32::from(*chunk.get(1).unwrap_or(&0));
        let b2 = u32::from(*chunk.get(2).unwrap_or(&0));
        let n = (b0 << 16) | (b1 << 8) | b2;
        for i in 0..=chunk.len() {
            out.push(B64URL[((n >> (18 - 6 * i)) & 63) as usize] as char);
        }
    }
    out
}

/// The bytes of unpadded base64url text, or `None` when it is not one.
pub fn b64url_decode(text: &str) -> Option<Vec<u8>> {
    if text.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in text.bytes() {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// The POST body of `/api/push/subscribe`: the endpoint and the two keys, plus
/// the invite when the page is an invited approver's. Nothing else: the bridge
/// refuses unknown fields.
pub fn subscribe_body(endpoint: &str, p256dh: &str, auth: &str, invite: Option<&str>) -> String {
    let mut body = json!({
        "endpoint": endpoint,
        "keys": { "p256dh": p256dh, "auth": auth },
    });
    if let Some(invite) = invite {
        body["invite"] = json!(invite);
    }
    body.to_string()
}

/// The DELETE body of `/api/push/subscribe`: the endpoint, plus the invite
/// for an approver.
pub fn unsubscribe_body(endpoint: &str, invite: Option<&str>) -> String {
    let mut body = json!({ "endpoint": endpoint });
    if let Some(invite) = invite {
        body["invite"] = json!(invite);
    }
    body.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> Facts {
        Facts {
            key_status: Some(200),
            role: Role::Owner,
            supported: true,
            ios_tab: false,
            permission: Permission::Default,
            subscribed: false,
        }
    }

    #[test]
    fn base64url_matches_the_rfc_4648_vectors() {
        let cases = [
            ("", ""),
            ("f", "Zg"),
            ("fo", "Zm8"),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg"),
            ("fooba", "Zm9vYmE"),
            ("foobar", "Zm9vYmFy"),
        ];
        for (raw, text) in cases {
            assert_eq!(b64url_encode(raw.as_bytes()), text);
            assert_eq!(b64url_decode(text).as_deref(), Some(raw.as_bytes()));
        }
    }

    #[test]
    fn base64url_uses_dash_and_underscore_and_refuses_other_text() {
        // 0xfb 0xff is "+/8" in standard base64: "-_8" here.
        assert_eq!(b64url_encode(&[0xfb, 0xff]), "-_8");
        assert_eq!(b64url_decode("-_8"), Some(vec![0xfb, 0xff]));
        assert_eq!(b64url_decode("+/8="), None);
        assert_eq!(b64url_decode("abcde"), None, "a length of 1 mod 4 is not base64");
        assert_eq!(b64url_decode("ab cd"), None);
    }

    #[test]
    fn a_vapid_public_key_round_trips_as_65_bytes_from_0x04() {
        let mut key = vec![0x04u8];
        key.extend((1..65u8).map(|b| b.wrapping_mul(7)));
        let text = b64url_encode(&key);
        assert_eq!(text.len(), 87, "65 bytes, unpadded");
        let back = b64url_decode(&text).expect("decodes");
        assert_eq!(back.len(), 65);
        assert_eq!(back[0], 0x04);
        assert_eq!(back, key);
    }

    #[test]
    fn state_follows_the_key_route_support_and_subscription() {
        assert_eq!(row_state(&facts()), RowState::Off);
        assert_eq!(row_state(&Facts { subscribed: true, ..facts() }), RowState::On);
        assert_eq!(row_state(&Facts { key_status: Some(503), ..facts() }), RowState::Hidden);
        assert_eq!(row_state(&Facts { key_status: Some(401), ..facts() }), RowState::Hidden);
        assert_eq!(row_state(&Facts { key_status: None, ..facts() }), RowState::Hidden);
        assert_eq!(row_state(&Facts { role: Role::Viewer, ..facts() }), RowState::Hidden);
        assert_eq!(row_state(&Facts { supported: false, ..facts() }), RowState::Hidden);
    }

    #[test]
    fn an_ios_tab_without_push_shows_the_row_unavailable_with_the_hint() {
        let tab = Facts { supported: false, ios_tab: true, ..facts() };
        assert_eq!(row_state(&tab), RowState::Unavailable);
        let shown = view(&tab, None).expect("row");
        assert_eq!(shown.line, "notify this device: unavailable");
        assert!(!shown.tappable);
        assert_eq!(shown.hint.as_deref(), Some(PUSH_HOME_SCREEN_HINT));
        assert!(view(&Facts { supported: false, ios_tab: false, ..facts() }, None).is_none());
    }

    #[test]
    fn a_blocked_permission_leaves_the_row_off_and_names_the_reason() {
        let blocked = Facts { permission: Permission::Denied, ..facts() };
        let shown = view(&blocked, None).expect("row");
        assert_eq!(shown.line, "notify this device: off");
        assert!(shown.tappable);
        assert_eq!(shown.note.as_deref(), Some(PUSH_BLOCKED_LABEL));
        // A tap's own failure takes the note's place.
        assert_eq!(
            view(&blocked, Some("could not change notifications")).unwrap().note.as_deref(),
            Some("could not change notifications")
        );
    }

    #[test]
    fn the_row_words_and_taps_for_on_and_off() {
        let on = view(&Facts { subscribed: true, ..facts() }, None).expect("row");
        assert_eq!(on.line, "notify this device: on");
        assert!(on.tappable && on.hint.is_none() && on.note.is_none());
        assert_eq!(view(&facts(), None).unwrap().line, "notify this device: off");
    }

    #[test]
    fn roles_come_from_the_key_answer_and_the_invite() {
        assert_eq!(role_of(Some(200), false), Role::Owner);
        assert_eq!(role_of(Some(200), true), Role::Approver);
        assert_eq!(role_of(Some(403), true), Role::Viewer);
        assert_eq!(role_of(None, false), Role::Viewer);
    }

    #[test]
    fn permission_words_map_and_unknown_is_default() {
        assert_eq!(Permission::parse("granted"), Permission::Granted);
        assert_eq!(Permission::parse("denied"), Permission::Denied);
        assert_eq!(Permission::parse("default"), Permission::Default);
        assert_eq!(Permission::parse(""), Permission::Default);
    }

    #[test]
    fn the_invite_is_read_from_the_page_address_only_when_plausible() {
        assert_eq!(
            invite_from_query("?url=ws%3A%2F%2Fh&invite=inv1.abc_-9").as_deref(),
            Some("inv1.abc_-9")
        );
        assert_eq!(invite_from_query("?invite=").as_deref(), None);
        assert_eq!(invite_from_query("?invite=a%2Fb").as_deref(), None, "a slash is refused");
        assert_eq!(invite_from_query("?url=x").as_deref(), None);
        assert_eq!(invite_from_query(&format!("?invite={}", "a".repeat(257))).as_deref(), None);
    }

    #[test]
    fn subscribe_and_unsubscribe_bodies_carry_only_their_fields() {
        let body: serde_json::Value =
            serde_json::from_str(&subscribe_body("https://fcm.googleapis.com/x", "P", "A", None))
                .unwrap();
        assert_eq!(
            body,
            json!({ "endpoint": "https://fcm.googleapis.com/x", "keys": { "p256dh": "P", "auth": "A" } })
        );
        let approver: serde_json::Value = serde_json::from_str(&subscribe_body(
            "https://fcm.googleapis.com/x",
            "P",
            "A",
            Some("inv1.s"),
        ))
        .unwrap();
        assert_eq!(approver["invite"], "inv1.s");
        let gone: serde_json::Value =
            serde_json::from_str(&unsubscribe_body("https://e", None)).unwrap();
        assert_eq!(gone, json!({ "endpoint": "https://e" }));
        let gone_as_approver: serde_json::Value =
            serde_json::from_str(&unsubscribe_body("https://e", Some("inv1.s"))).unwrap();
        assert_eq!(gone_as_approver, json!({ "endpoint": "https://e", "invite": "inv1.s" }));
    }

    #[test]
    fn ios_devices_are_recognised_by_their_user_agent() {
        assert!(is_ios("Mozilla/5.0 (iPhone; CPU iPhone OS 17_0 like Mac OS X)"));
        assert!(is_ios("Mozilla/5.0 (iPad; CPU OS 17_0 like Mac OS X)"));
        assert!(!is_ios("Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0)"));
    }
}
