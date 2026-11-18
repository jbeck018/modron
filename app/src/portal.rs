//! The owner portal (Packet C4g), target-neutral: the page's mode from
//! `GET /api/me`, the wire rows of `/api/sessions` and the invites, and the
//! portal list's open rule. The HTTP calls and the page's timer live in
//! `web.rs` (wasm32 only). The words come from the client core.

use modron_mobile_core::view::{
    portal_session_parts, SIGN_IN_FAILED_LABEL, TOO_MANY_ATTEMPTS_LABEL, WRONG_TOKEN_LABEL,
};
use serde::Deserialize;

/// What a page with no `url` parameter shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// A `url` parameter: the page connects to that server directly.
    Live,
    /// The built-in demo (the portal is off, or this is not a portal page).
    Demo,
    /// The owner sign-in screen.
    SignIn,
    /// The owner's session list.
    Portal,
}

/// The mode from the `GET /api/me` status (same origin, cookie included):
/// 200 is the owner's portal, 401 the sign-in screen. Anything else (503 when
/// the portal is off, or no answer at all) is the demo, as before the portal.
pub fn mode_without_url(me_status: Option<u16>) -> Mode {
    match me_status {
        Some(200) => Mode::Portal,
        Some(401) => Mode::SignIn,
        _ => Mode::Demo,
    }
}

/// The page's mode: a `url` parameter connects directly; without one the
/// `/api/me` status decides (see [`mode_without_url`]).
pub fn decide(has_url: bool, me_status: Option<u16>) -> Mode {
    if has_url {
        Mode::Live
    } else {
        mode_without_url(me_status)
    }
}

/// The sign-in message for a `POST /auth` answer; `None` when it signed in.
pub fn sign_in_error(status: u16) -> Option<&'static str> {
    match status {
        200 => None,
        401 => Some(WRONG_TOKEN_LABEL),
        429 => Some(TOO_MANY_ATTEMPTS_LABEL),
        _ => Some(SIGN_IN_FAILED_LABEL),
    }
}

/// One `GET /api/sessions` row. Unknown fields are ignored.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct PortalRow {
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub host_label: String,
    #[serde(default)]
    pub online: bool,
    #[serde(default)]
    pub updated_at: String,
    #[serde(default)]
    pub open_url: String,
}

/// The rows of a `GET /api/sessions` body (a JSON array), or `None` when
/// the body is not one.
pub fn parse_sessions(body: &str) -> Option<Vec<PortalRow>> {
    serde_json::from_str(body).ok()
}

/// One `GET /api/invites` row.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct InviteRow {
    pub id: String,
    #[serde(default)]
    pub role: String,
    #[serde(default)]
    pub expires_at: String,
    #[serde(default)]
    pub revoked: bool,
}

/// The invites of a `GET /api/invites` body that still work (revoked ones
/// are left out), or `None` when the body is not a list.
pub fn parse_invites(body: &str) -> Option<Vec<InviteRow>> {
    let rows: Vec<InviteRow> = serde_json::from_str(body).ok()?;
    Some(rows.into_iter().filter(|r| !r.revoked).collect())
}

/// One portal list row as the screen shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PortalLine {
    pub text: String,
    /// The title, and the subtitle (host, online or offline, time): the web
    /// draws them on two lines. `text` is their join (the terminal's line).
    pub title: String,
    pub subtitle: String,
    pub online: bool,
    pub open_url: String,
}

/// The list rows, in the server's order (newest first).
pub fn lines(rows: &[PortalRow]) -> Vec<PortalLine> {
    rows.iter()
        .map(|row| {
            let (title, subtitle) =
                portal_session_parts(&row.title, &row.host_label, row.online, &row.updated_at);
            PortalLine {
                text: format!("{title} \u{b7} {subtitle}"),
                title,
                subtitle,
                online: row.online,
                open_url: row.open_url.clone(),
            }
        })
        .collect()
}

/// The URL a press opens: only an online row with a same-origin path (the
/// bridge's `/app/?url=...`). An offline row, or anything else, opens nothing.
pub fn open_target(line: &PortalLine) -> Option<String> {
    let url = line.open_url.as_str();
    (line.online && url.starts_with('/') && !url.starts_with("//")).then(|| url.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSIONS: &str = r#"[
        {"id":"a","title":"fix router","host_label":"laptop","host_kind":"local",
         "viewer_key":"k","state":"idle","online":true,
         "updated_at":"2026-11-15T10:20:30Z","open_url":"/app/?url=ws%3A%2F%2Fh%2Fws%2Fk"},
        {"id":"b","title":"","host_label":"cloud","host_kind":"cloud","state":"offline",
         "online":false,"updated_at":"2026-11-14T08:00:00Z","open_url":"/app/?url=x"}
    ]"#;

    #[test]
    fn no_url_mode_follows_the_me_status() {
        assert_eq!(mode_without_url(Some(200)), Mode::Portal);
        assert_eq!(mode_without_url(Some(401)), Mode::SignIn);
        assert_eq!(mode_without_url(Some(503)), Mode::Demo);
        assert_eq!(mode_without_url(Some(404)), Mode::Demo);
        assert_eq!(mode_without_url(None), Mode::Demo);
    }

    #[test]
    fn a_url_parameter_connects_directly_whatever_the_portal_says() {
        assert_eq!(decide(true, Some(200)), Mode::Live);
        assert_eq!(decide(true, Some(401)), Mode::Live);
        assert_eq!(decide(true, Some(503)), Mode::Live);
        assert_eq!(decide(false, Some(503)), Mode::Demo);
        assert_eq!(decide(false, Some(401)), Mode::SignIn);
        assert_eq!(decide(false, Some(200)), Mode::Portal);
    }

    #[test]
    fn sign_in_answers_map_to_the_shared_words() {
        assert_eq!(sign_in_error(200), None);
        assert_eq!(sign_in_error(401), Some("wrong token"));
        assert_eq!(sign_in_error(429), Some("too many attempts"));
        assert_eq!(sign_in_error(500), Some("sign-in failed"));
    }

    #[test]
    fn session_rows_parse_and_ignore_unknown_fields() {
        let rows = parse_sessions(SESSIONS).expect("array");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].title, "fix router");
        assert!(rows[0].online);
        assert!(parse_sessions("{\"sessions\":[]}").is_none());
        assert!(parse_sessions("not json").is_none());
    }

    #[test]
    fn lines_render_one_row_each_and_only_online_rows_open() {
        let rows = parse_sessions(SESSIONS).unwrap();
        let lines = lines(&rows);
        assert_eq!(lines[0].text, "fix router \u{b7} laptop \u{b7} online \u{b7} 2026-11-15 10:20");
        assert_eq!(lines[1].text, "untitled \u{b7} cloud \u{b7} offline \u{b7} 2026-11-14 08:00");
        assert_eq!(open_target(&lines[0]).as_deref(), Some("/app/?url=ws%3A%2F%2Fh%2Fws%2Fk"));
        assert_eq!(open_target(&lines[1]), None);
    }

    #[test]
    fn only_same_origin_paths_open() {
        let line = |url: &str| PortalLine {
            text: String::new(),
            title: String::new(),
            subtitle: String::new(),
            online: true,
            open_url: url.into(),
        };
        assert_eq!(open_target(&line("//evil.example/x")), None);
        assert_eq!(open_target(&line("https://evil.example/")), None);
        assert_eq!(open_target(&line("")), None);
        assert_eq!(open_target(&line("/app/?url=y")).as_deref(), Some("/app/?url=y"));
    }

    #[test]
    fn revoked_invites_are_left_out() {
        let body = r#"[{"id":"i1","role":"viewer","expires_at":"2026-11-16T00:00:00Z","revoked":false,"url":"u"},
                       {"id":"i2","role":"approver","expires_at":"2026-11-16T00:00:00Z","revoked":true}]"#;
        let rows = parse_invites(body).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, "i1");
    }
}
