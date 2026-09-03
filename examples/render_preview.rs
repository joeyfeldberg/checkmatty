//! Render the board compositor to a PNG for visual review.

use std::{env, error::Error, path::PathBuf};

use chess_tui::render::{
    BoardScene, LastMove, RasterOptions, RasterPieceStyle, Square, TargetKind, compose_board_rgba,
};

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args_os().skip(1);
    let output = arguments
        .next()
        .map_or_else(|| PathBuf::from("chess-preview.png"), PathBuf::from);
    let piece_style = match arguments.next().as_deref().and_then(|value| value.to_str()) {
        None | Some("detailed") => RasterPieceStyle::Detailed,
        Some("pixel") => RasterPieceStyle::PixelArt,
        Some(other) => {
            return Err(format!("unknown piece style {other:?}; use detailed or pixel").into());
        }
    };
    let side_px = arguments
        .next()
        .and_then(|value| value.to_string_lossy().parse().ok())
        .unwrap_or(768);
    let mut scene = BoardScene::starting_position();
    let d2 = "d2".parse::<Square>()?;
    let d4 = "d4".parse::<Square>()?;
    let pawn = scene.piece_at(d2);
    scene.set_piece(d2, None);
    scene.set_piece(d4, pawn);
    scene.cursor = Some("e2".parse::<Square>()?);
    scene.selected = Some("g1".parse::<Square>()?);
    scene.set_target("f3".parse::<Square>()?, Some(TargetKind::Quiet));
    scene.set_target("h3".parse::<Square>()?, Some(TargetKind::Capture));
    scene.last_move = Some(LastMove::new(d2, d4));

    compose_board_rgba(
        &scene,
        &RasterOptions {
            side_px,
            piece_style,
            ..RasterOptions::default()
        },
    )
    .save(&output)?;
    println!("wrote {}", output.display());
    Ok(())
}
