#[tokio::main]
async fn main() -> anyhow::Result<()> {
    marketplace::app().serve().await
}
