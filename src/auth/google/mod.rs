pub mod flow;
pub mod store;

pub use store::OAuthClientConfig;

/// Scope requested for the Google Calendar integration. Read-only is enough for
/// glint's calendar widget — we never write events.
pub const SCOPE: &str = "https://www.googleapis.com/auth/calendar.readonly";

pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
