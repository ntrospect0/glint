pub mod flow;
pub mod store;

pub use store::OAuthClientConfig;

/// Scopes requested for Microsoft Graph. `Calendars.Read` powers the calendar
/// widget, `Mail.Read` powers the Email widget (read-only — glint never marks
/// or modifies server-side state). `offline_access` is what gets us a refresh
/// token from Microsoft's identity platform.
///
/// Tokens issued against the old (Calendars-only) scope will keep working for
/// the calendar widget; the Email widget needs the user to re-run
/// `glint --auth outlook` so the additional Mail.Read scope is granted.
pub const SCOPE: &str = "Calendars.Read Mail.Read offline_access";

/// `common` accepts both personal Microsoft accounts (outlook.com /
/// hotmail.com) and work/school accounts. The Azure app registration must
/// allow the matching account types.
pub const AUTH_URL: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/authorize";
pub const TOKEN_URL: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/token";
