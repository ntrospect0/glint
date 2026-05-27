pub mod flow;
pub mod store;

pub use store::OAuthClientConfig;

/// Scopes requested for Google integrations. Calendar.readonly powers the
/// calendar widget; gmail.readonly powers the Email widget. Read-only on both
/// — glint never writes events or modifies messages on the server.
///
/// Google accepts a space-separated scope list in the OAuth `scope` parameter.
/// Existing tokens issued against the old single-scope value will still work
/// for the calendar widget; to enable the Email widget the user must re-run
/// `glint --auth google` so the new scope is granted.
pub const SCOPE: &str = "https://www.googleapis.com/auth/calendar.readonly https://www.googleapis.com/auth/gmail.readonly";

pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
