#[tokio::main]
async fn main() -> anyhow::Result<()> {
    nutrition::app().serve().await
}
