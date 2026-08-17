#[tokio::main]
async fn main() -> anyhow::Result<()> {
    osrs_wiki_offline::run().await
}
