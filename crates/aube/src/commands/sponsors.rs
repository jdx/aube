use clap::Args;

#[derive(Debug, Args)]
pub struct SponsorsArgs {}

pub async fn run(_args: SponsorsArgs) -> miette::Result<()> {
    println!(
        "aube and the en.dev project family are sponsored by:\n\n  37signals - https://37signals.com\n\nView all sponsors: https://en.dev/sponsors.html"
    );
    Ok(())
}
