#[tokio::main]
async fn main() -> anyhow::Result<()> {
    feedback::app().serve().await
}
