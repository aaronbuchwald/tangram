#[tokio::main]
async fn main() -> anyhow::Result<()> {
    auto_todo::app().serve().await
}
