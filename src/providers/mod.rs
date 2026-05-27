use anyhow::Result;
use async_trait::async_trait;

/// A `DataProvider` fetches some payload of type `Data` from an external source.
/// Providers are owned by widgets; the widget decides when (and how often) to
/// call `fetch` and how to render the result.
#[allow(dead_code)] // implemented in Phase 2+ (Yahoo Finance, Google Calendar, RSS).
#[async_trait]
pub trait DataProvider: Send + Sync {
    type Data: Send;

    async fn fetch(&self) -> Result<Self::Data>;

    fn name(&self) -> &str;
}
