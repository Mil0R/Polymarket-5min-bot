use anyhow::Result;
use polypulse::app::{self, RuntimeProfile};

#[tokio::main]
async fn main() -> Result<()> {
    app::run(RuntimeProfile::Integrated).await
}
