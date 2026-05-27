pub mod flow;
pub mod store;

pub use store::OAuthClientConfig;

/// Scopes requested for Microsoft Graph. `Calendars.Read` powers the calendar
/// widget, `Mail.Read` powers the Email widget (read-only — glint never marks
/// or modifies server-side state). `User.Read` is required for `/me` to
/// return the signed-in account's email address; without it the email
/// widget's title row shows "(loading…)" forever. `offline_access` is what
/// gets us a refresh token from Microsoft's identity platform.
///
/// Tokens issued under older scope strings keep working for whatever they
/// already cover; users who upgrade to a build with new scopes need to
/// re-authorize (Wizard → Authorize Microsoft, or `glint --auth microsoft`).
pub const SCOPE: &str = "Calendars.Read Mail.Read User.Read offline_access";

/// `common` accepts both personal Microsoft accounts (outlook.com /
/// hotmail.com) and work/school accounts. The Azure app registration must
/// allow the matching account types.
pub const AUTH_URL: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/authorize";
pub const TOKEN_URL: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/token";
