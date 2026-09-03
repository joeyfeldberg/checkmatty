use chess_tui::{cli::Cli, terminal::run};
use clap::Parser;
use color_eyre::eyre::Result;

fn main() -> Result<()> {
    color_eyre::install()?;
    run(Cli::parse())
}
