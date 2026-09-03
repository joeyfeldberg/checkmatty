use chess_tui::chess::{
    CastleSide, Color, ColoredPiece, DrawReason, Game, GameStatus, Move, MoveError, Piece, Square,
};
use pretty_assertions::assert_eq;

fn play(game: &mut Game, notation: &str) {
    game.play_move(notation.parse().unwrap()).unwrap();
}

#[test]
fn coordinate_moves_parse_and_display_canonically() {
    let quiet: Move = "e2e4".parse().unwrap();
    assert_eq!(quiet, Move::new(Square::E2, Square::E4, None));
    assert_eq!(quiet.to_string(), "e2e4");

    let promotion: Move = "a7a8Q".parse().unwrap();
    assert_eq!(promotion.promotion, Some(Piece::Queen));
    assert_eq!(promotion.to_string(), "a7a8q");

    for invalid in ["", "e2", "e2e4qq", "e2e4k", "e2e4p", "é2e4"] {
        assert!(invalid.parse::<Move>().is_err(), "accepted {invalid:?}");
    }
}

#[test]
fn standard_position_exposes_board_and_legal_move_accessors() {
    let game = Game::new();

    assert_eq!(game.side_to_move(), Color::White);
    assert_eq!(game.status(), GameStatus::Ongoing);
    assert_eq!(game.pieces().count(), 32);
    assert_eq!(
        game.piece_at(Square::E1),
        Some(ColoredPiece {
            color: Color::White,
            piece: Piece::King,
        })
    );
    assert_eq!(
        game.piece_at(Square::E8),
        Some(ColoredPiece {
            color: Color::Black,
            piece: Piece::King,
        })
    );
    assert_eq!(game.king_square(Color::White), Square::E1);
    assert_eq!(game.legal_moves().len(), 20);
    assert_eq!(
        game.legal_destinations_from(Square::E2),
        vec![Square::E3, Square::E4]
    );
    assert!(game.is_legal_move("e2e4".parse().unwrap()));
    assert!(!game.is_legal_move("e2e5".parse().unwrap()));
    assert_eq!(
        game.castle_rights(Color::White),
        chess_tui::chess::CastlingRights {
            kingside: true,
            queenside: true,
        }
    );
}

#[test]
fn play_history_undo_and_reset_restore_all_position_state() {
    let mut game = Game::new();
    let start_fen = game.fen();

    let first = game.play(Square::E2, Square::E4, None).unwrap();
    assert_eq!(first.mv.to_string(), "e2e4");
    assert_eq!(first.san, "e4");
    assert_eq!(first.color, Color::White);
    assert_eq!(first.piece, Piece::Pawn);
    assert!(!first.is_capture());
    assert_eq!(game.en_passant_target(), Some(Square::E3));
    assert_eq!(game.side_to_move(), Color::Black);
    assert_eq!(game.ply_count(), 1);
    assert_eq!(game.last_move(), Some(&first));

    play(&mut game, "e7e5");
    play(&mut game, "g1f3");
    let before_undo = game.fen();
    assert_ne!(before_undo, start_fen);
    let undone = game.undo().unwrap();
    assert_eq!(undone.mv.to_string(), "g1f3");
    assert_eq!(game.side_to_move(), Color::White);
    assert_eq!(game.history().len(), 2);
    assert_eq!(game.piece_at(Square::G1).unwrap().piece, Piece::Knight);
    assert!(game.can_undo());

    game.reset();
    assert_eq!(game.fen(), start_fen);
    assert!(game.history().is_empty());
    assert!(!game.can_undo());
    assert_eq!(game.repetition_count(), 1);
}

#[test]
fn fen_loading_is_atomic_and_sets_the_reset_position() {
    let custom = "4k3/8/8/8/8/8/4P3/4K3 w - - 0 1";
    let mut game = Game::from_fen(custom).unwrap();
    assert_eq!(game.initial_fen(), custom);
    play(&mut game, "e2e4");
    game.reset();
    assert_eq!(game.fen(), custom);

    let before = game.fen();
    assert!(game.load_fen("not a fen").is_err());
    assert_eq!(game.fen(), before);

    game.reset_to_start_position();
    assert_eq!(game, Game::new());
}

#[test]
fn castling_uses_the_kings_visible_destination_and_moves_the_rook() {
    let mut game = Game::from_fen("r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1").unwrap();
    let moves = game.legal_moves_from(Square::E1);
    assert!(moves.contains(&"e1g1".parse().unwrap()));
    assert!(moves.contains(&"e1c1".parse().unwrap()));
    // The cozy-chess king-to-rook encoding never leaks through the adapter.
    assert!(!moves.iter().any(|mv| mv.to == Square::H1));
    assert!(!moves.iter().any(|mv| mv.to == Square::A1));

    let record = game.play(Square::E1, Square::G1, None).unwrap();
    assert_eq!(record.castle, Some(CastleSide::Kingside));
    assert_eq!(record.san, "O-O");
    assert!(record.is_castle());
    assert_eq!(game.piece_at(Square::G1).unwrap().piece, Piece::King);
    assert_eq!(game.piece_at(Square::F1).unwrap().piece, Piece::Rook);
    assert_eq!(game.piece_at(Square::E1), None);
    assert_eq!(game.piece_at(Square::H1), None);
    assert!(!game.has_castle_right(Color::White, CastleSide::Kingside));
    assert!(!game.has_castle_right(Color::White, CastleSide::Queenside));

    game.undo().unwrap();
    let record = game.play(Square::E1, Square::C1, None).unwrap();
    assert_eq!(record.castle, Some(CastleSide::Queenside));
    assert_eq!(record.san, "O-O-O");
    assert_eq!(game.piece_at(Square::C1).unwrap().piece, Piece::King);
    assert_eq!(game.piece_at(Square::D1).unwrap().piece, Piece::Rook);
}

#[test]
fn castling_through_an_attacked_square_is_not_legal() {
    let game = Game::from_fen("r3kr2/8/8/8/8/8/8/R3K2R w KQ - 0 1").unwrap();
    let moves = game.legal_moves_from(Square::E1);
    assert!(!moves.contains(&"e1g1".parse().unwrap()));
    assert!(moves.contains(&"e1c1".parse().unwrap()));
}

#[test]
fn en_passant_removes_and_records_the_pawn_on_its_actual_square() {
    let mut game = Game::from_fen("4k3/8/8/3pP3/8/8/8/4K3 w - d6 0 1").unwrap();
    assert_eq!(game.en_passant_target(), Some(Square::D6));
    assert!(
        game.legal_moves_from(Square::E5)
            .contains(&"e5d6".parse().unwrap())
    );

    let record = game.play(Square::E5, Square::D6, None).unwrap();
    assert_eq!(record.san, "exd6");
    assert_eq!(
        record.captured,
        Some(ColoredPiece {
            color: Color::Black,
            piece: Piece::Pawn,
        })
    );
    assert_eq!(record.capture_square, Some(Square::D5));
    assert_eq!(game.piece_at(Square::D5), None);
    assert_eq!(game.piece_at(Square::D6).unwrap().piece, Piece::Pawn);
    assert_eq!(game.halfmove_clock(), 0);
}

#[test]
fn promotion_requires_an_explicit_valid_piece_and_records_check() {
    let mut game = Game::from_fen("4k3/P7/8/8/8/8/8/4K3 w - - 0 1").unwrap();
    let promotions = game.legal_moves_from(Square::A7);
    assert_eq!(promotions.len(), 4);
    assert!(promotions.iter().all(|mv| mv.to == Square::A8));
    assert_eq!(game.legal_destinations_from(Square::A7), vec![Square::A8]);

    assert_eq!(
        game.find_move(Square::A7, Square::A8, None),
        Err(MoveError::PromotionRequired {
            from: Square::A7,
            to: Square::A8,
        })
    );
    assert_eq!(
        game.find_move(Square::A7, Square::A8, Some(Piece::King)),
        Err(MoveError::InvalidPromotion(Piece::King))
    );

    let record = game
        .play(Square::A7, Square::A8, Some(Piece::Queen))
        .unwrap();
    assert_eq!(record.san, "a8=Q+");
    assert!(record.is_promotion());
    assert!(record.gave_check);
    assert_eq!(game.piece_at(Square::A8).unwrap().piece, Piece::Queen);
}

#[test]
fn check_checkmate_and_stalemate_are_distinct() {
    let checked = Game::from_fen("4k3/8/8/8/8/8/4R3/4K3 b - - 0 1").unwrap();
    assert_eq!(checked.status(), GameStatus::Check);
    assert!(checked.in_check());
    assert!(!checked.is_game_over());

    let stalemate = Game::from_fen("7k/5Q2/6K1/8/8/8/8/8 b - - 0 1").unwrap();
    assert_eq!(stalemate.status(), GameStatus::Stalemate);
    assert!(stalemate.is_stalemate());
    assert!(!stalemate.in_check());
    assert!(stalemate.legal_moves().is_empty());

    let mut mate = Game::new();
    for mv in ["f2f3", "e7e5", "g2g4"] {
        play(&mut mate, mv);
    }
    let record = mate.play_move("d8h4".parse().unwrap()).unwrap();
    assert_eq!(record.san, "Qh4#");
    assert!(record.gave_checkmate);
    assert_eq!(
        mate.status(),
        GameStatus::Checkmate {
            winner: Color::Black,
        }
    );
    assert!(mate.is_checkmate());
    assert!(mate.legal_moves().is_empty());
    assert_eq!(
        mate.play(Square::A2, Square::A3, None),
        Err(MoveError::GameOver(GameStatus::Checkmate {
            winner: Color::Black,
        }))
    );
}

#[test]
fn threefold_repetition_is_tracked_and_undoable() {
    let mut game = Game::new();
    for mv in [
        "g1f3", "g8f6", "f3g1", "f6g8", "g1f3", "g8f6", "f3g1", "f6g8",
    ] {
        play(&mut game, mv);
    }

    assert_eq!(game.repetition_count(), 3);
    assert_eq!(
        game.status(),
        GameStatus::Draw {
            reason: DrawReason::ThreefoldRepetition,
        }
    );
    assert!(game.legal_moves().is_empty());

    game.undo().unwrap();
    assert_eq!(game.repetition_count(), 2);
    assert_eq!(game.status(), GameStatus::Ongoing);
    play(&mut game, "f6g8");
    assert_eq!(game.repetition_count(), 3);
}

#[test]
fn fifty_move_clock_is_exposed_and_adjudicated() {
    let mut game = Game::from_fen("4k2r/8/8/8/8/8/8/R3K3 w - - 99 1").unwrap();
    assert_eq!(game.halfmove_clock(), 99);
    assert_eq!(game.plies_until_fifty_move_rule(), 1);
    assert_eq!(game.status(), GameStatus::Ongoing);

    play(&mut game, "a1a2");
    assert_eq!(game.halfmove_clock(), 100);
    assert_eq!(game.plies_until_fifty_move_rule(), 0);
    assert_eq!(
        game.status(),
        GameStatus::Draw {
            reason: DrawReason::FiftyMoveRule,
        }
    );
}

#[test]
fn common_dead_material_positions_are_draws() {
    for fen in [
        "4k3/8/8/8/8/8/8/4K3 w - - 0 1",
        "4k3/8/8/8/8/8/8/2B1K3 w - - 0 1",
        "4kb2/8/8/8/8/8/8/2B1K3 w - - 0 1",
    ] {
        let game = Game::from_fen(fen).unwrap();
        assert_eq!(
            game.status(),
            GameStatus::Draw {
                reason: DrawReason::InsufficientMaterial,
            },
            "{fen}"
        );
    }

    // Bishops on opposite color complexes can participate in a mating sequence.
    let opposite_bishops = Game::from_fen("2b1k3/8/8/8/8/8/8/2B1K3 w - - 0 1").unwrap();
    assert_eq!(opposite_bishops.status(), GameStatus::Ongoing);
}

#[test]
fn san_disambiguates_two_identical_pieces() {
    let mut game = Game::from_fen("4k3/8/8/8/8/8/8/1N2KN2 w - - 0 1").unwrap();
    let record = game.play(Square::B1, Square::D2, None).unwrap();
    assert_eq!(record.san, "Nbd2");
}
