#[tokio::main]
async fn main() -> anyhow::Result<()> {
    nutrition::app().serve_with(nutrition::with_api).await
}
