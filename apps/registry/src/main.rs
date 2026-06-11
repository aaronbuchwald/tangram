#[tokio::main]
async fn main() -> anyhow::Result<()> {
    registry::app().serve().await
}
