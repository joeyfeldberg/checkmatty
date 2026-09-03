use std::str::FromStr;

use clap::{Parser, ValueEnum};

#[derive(Debug, Clone, Parser)]
#[command(author, version, about)]
pub struct Cli {
    /// Select the board renderer. Auto prefers terminal graphics and falls back safely.
    #[arg(long, value_enum, default_value_t = GraphicsMode::Auto)]
    pub graphics: GraphicsMode,

    /// Disable visual move transitions.
    #[arg(long)]
    pub no_animation: bool,

    /// Play against a UCI engine. Use `maia` for the app-managed human-like Maia-3 preset.
    #[arg(long, value_name = "COMMAND")]
    pub engine: Option<String>,

    /// Append one argument when launching the external engine.
    #[arg(long = "engine-arg", value_name = "ARG", requires = "engine")]
    pub engine_args: Vec<String>,

    /// Set a UCI option as NAME=VALUE. May be repeated.
    #[arg(long = "engine-option", value_name = "NAME=VALUE", requires = "engine")]
    pub engine_options: Vec<EngineOption>,

    /// Side controlled by the human in an engine game.
    #[arg(
        long,
        value_enum,
        default_value_t = HumanColor::White,
        requires = "engine"
    )]
    pub human_color: HumanColor,

    /// Human rating the Maia preset should imitate (default: 1500).
    #[arg(
        long,
        value_parser = clap::value_parser!(u16).range(600..=2600),
        requires = "engine"
    )]
    pub engine_elo: Option<u16>,

    /// Maia sampling temperature (default: 1; zero is deterministic).
    #[arg(long, value_parser = parse_temperature, requires = "engine")]
    pub engine_temperature: Option<f32>,

    /// Maia nucleus-sampling probability (default: 1).
    #[arg(long, value_parser = parse_top_p, requires = "engine")]
    pub engine_top_p: Option<f32>,

    /// Search a fixed node count instead of using the engine preset default.
    #[arg(
        long,
        value_parser = clap::value_parser!(u64).range(1..),
        conflicts_with = "engine_movetime_ms",
        requires = "engine"
    )]
    pub engine_nodes: Option<u64>,

    /// Think for a fixed number of milliseconds instead of using the engine preset default.
    #[arg(
        long,
        value_parser = clap::value_parser!(u64).range(1..),
        conflicts_with = "engine_nodes",
        requires = "engine"
    )]
    pub engine_movetime_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineOption {
    pub name: String,
    pub value: String,
}

impl FromStr for EngineOption {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (name, value) = value
            .split_once('=')
            .ok_or_else(|| "expected NAME=VALUE".to_owned())?;
        let name = name.trim();
        let value = value.trim();
        if name.is_empty() || value.is_empty() {
            return Err("engine option name and value must be non-empty".to_owned());
        }
        Ok(Self {
            name: name.to_owned(),
            value: value.to_owned(),
        })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum HumanColor {
    #[default]
    White,
    Black,
}

fn parse_temperature(value: &str) -> Result<f32, String> {
    let temperature = value
        .parse::<f32>()
        .map_err(|_| "temperature must be a number".to_owned())?;
    if temperature.is_finite() && (0.0..=5.0).contains(&temperature) {
        Ok(temperature)
    } else {
        Err("temperature must be between 0 and 5".to_owned())
    }
}

fn parse_top_p(value: &str) -> Result<f32, String> {
    let top_p = value
        .parse::<f32>()
        .map_err(|_| "top-p must be a number".to_owned())?;
    if top_p.is_finite() && top_p > 0.0 && top_p <= 1.0 {
        Ok(top_p)
    } else {
        Err("top-p must be greater than 0 and at most 1".to_owned())
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum GraphicsMode {
    #[default]
    Auto,
    Kitty,
    Halfblocks,
    Unicode,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maia_engine_arguments_parse_with_human_side_and_sampling() {
        let cli = Cli::try_parse_from([
            "chess-tui",
            "--engine",
            "maia",
            "--human-color",
            "black",
            "--engine-elo",
            "1750",
            "--engine-temperature",
            "0.8",
            "--engine-top-p",
            "0.95",
            "--engine-option",
            "MultiPV=3",
        ])
        .unwrap();

        assert_eq!(cli.engine.as_deref(), Some("maia"));
        assert_eq!(cli.human_color, HumanColor::Black);
        assert_eq!(cli.engine_elo, Some(1750));
        assert_eq!(cli.engine_temperature, Some(0.8));
        assert_eq!(cli.engine_top_p, Some(0.95));
        assert_eq!(
            cli.engine_options,
            vec![EngineOption {
                name: "MultiPV".to_owned(),
                value: "3".to_owned(),
            }]
        );
    }

    #[test]
    fn invalid_engine_options_and_sampling_are_rejected() {
        assert!(
            Cli::try_parse_from(["chess-tui", "--engine", "maia", "--engine-option", "Elo"])
                .is_err()
        );
        assert!(
            Cli::try_parse_from(["chess-tui", "--engine", "maia", "--engine-top-p", "0"]).is_err()
        );
        assert!(
            Cli::try_parse_from(["chess-tui", "--engine", "maia", "--engine-elo", "3000"]).is_err()
        );
        assert!(Cli::try_parse_from(["chess-tui"]).is_ok());
        assert!(Cli::try_parse_from(["chess-tui", "--engine-elo", "1500"]).is_err());
        assert!(Cli::try_parse_from(["chess-tui", "--human-color", "black"]).is_err());
    }
}
