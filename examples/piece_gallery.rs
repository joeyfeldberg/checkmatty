//! Render every detailed piece on both board colors at the runtime Kitty density.

use std::{env, error::Error, path::PathBuf};

use chess_tui::render::{
    BoardScene, PieceKind, RasterOptions, RasterPieceStyle, Side, Square, ViewPiece,
    compose_board_rgba,
};

const PIECES: [PieceKind; 6] = [
    PieceKind::Pawn,
    PieceKind::Knight,
    PieceKind::Bishop,
    PieceKind::Rook,
    PieceKind::Queen,
    PieceKind::King,
];

fn main() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args_os().skip(1);
    let output = arguments
        .next()
        .map_or_else(|| PathBuf::from("chess-piece-gallery.png"), PathBuf::from);
    let side_px = arguments
        .next()
        .and_then(|value| value.to_string_lossy().parse().ok())
        .unwrap_or(768);

    let mut scene = BoardScene::empty();
    scene.show_coordinates = false;
    for (file, kind) in PIECES.into_iter().enumerate() {
        let file = u8::try_from(file)?;
        for rank in [6, 7] {
            scene.set_piece(
                Square::new(file, rank).expect("gallery square is valid"),
                Some(ViewPiece::new(Side::Black, kind)),
            );
        }
        for rank in [4, 5] {
            scene.set_piece(
                Square::new(file, rank).expect("gallery square is valid"),
                Some(ViewPiece::new(Side::White, kind)),
            );
        }
    }

    let board = compose_board_rgba(
        &scene,
        &RasterOptions {
            side_px,
            show_coordinates: false,
            piece_style: RasterPieceStyle::Detailed,
            ..RasterOptions::default()
        },
    );
    image::imageops::crop_imm(&board, 0, 0, board.width(), board.height() / 2)
        .to_image()
        .save(&output)?;
    println!("wrote {}", output.display());
    Ok(())
}
