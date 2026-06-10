#[tokio::main]
async fn main() -> anyhow::Result<()> {
    notes::app().serve().await
}
