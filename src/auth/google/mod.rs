pub mod flow;
pub mod store;

pub use store::OAuthClientConfig;

/// Space-separated OAuth scopes requested from Google. `calendar.readonly`
/// powers the calendar widget; `gmail.readonly` powers the email widget.
/// Both are read-only — glint never writes events or modifies messages.
pub const SCOPE: &str = "https://www.googleapis.com/auth/calendar.readonly https://www.googleapis.com/auth/gmail.readonly";

pub const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
pub const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
