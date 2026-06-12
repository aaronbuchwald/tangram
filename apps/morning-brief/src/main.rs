#[tokio::main]
async fn main() -> anyhow::Result<()> {
    morning_brief::app()
        .serve_with(morning_brief::with_api)
        .await
}
