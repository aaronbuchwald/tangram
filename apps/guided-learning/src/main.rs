#[tokio::main]
async fn main() -> anyhow::Result<()> {
    guided_learning::app().serve().await
}
