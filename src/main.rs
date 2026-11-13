#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    kubeweasel::run().await
}
