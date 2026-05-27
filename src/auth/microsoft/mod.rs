pub mod flow;
pub mod store;

pub use store::OAuthClientConfig;

/// Scopes requested for the Outlook calendar integration. `offline_access`
/// is what gets us a refresh token from Microsoft's identity platform.
pub const SCOPE: &str = "Calendars.Read offline_access";

/// `common` accepts both personal Microsoft accounts (outlook.com /
/// hotmail.com) and work/school accounts. The Azure app registration must
/// allow the matching account types.
pub const AUTH_URL: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/authorize";
pub const TOKEN_URL: &str = "https://login.microsoftonline.com/common/oauth2/v2.0/token";
