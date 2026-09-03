use chess_tui::{
    app::{App, Command, CommandResult, EngineMoveError, EngineSetupState, PendingPromotion},
    chess::{ChessMove, Color, GameStatus, Piece, Square},
    render::Orientation,
};
use pretty_assertions::assert_eq;

fn dispatch_move(app: &mut App, notation: &str) {
    let chess_move: ChessMove = notation.parse().expect("test move should parse");
    let ply_before = app.game().ply_count();

    assert_eq!(
        app.dispatch(Command::ActivateSquare(chess_move.from)),
        CommandResult::Continue
    );
    assert_eq!(app.selected(), Some(chess_move.from));
    assert_eq!(
        app.dispatch(Command::ActivateSquare(chess_move.to)),
        CommandResult::Continue
    );
    assert_eq!(
        app.game().ply_count(),
        ply_before + 1,
        "{notation} should be accepted by the reducer"
    );
    assert_eq!(
        app.game().last_move().map(|record| record.mv),
        Some(chess_move)
    );
}

#[test]
fn cursor_stays_on_the_board_and_follows_the_view_orientation() {
    let mut app = App::new();
    assert_eq!(app.cursor(), Square::E2);
    assert_eq!(app.orientation(), Orientation::White);

    app.dispatch(Command::MoveCursor {
        file_delta: -4,
        rank_delta: -1,
    });
    assert_eq!(app.cursor(), Square::A1);

    app.dispatch(Command::MoveCursor {
        file_delta: -1,
        rank_delta: -1,
    });
    assert_eq!(app.cursor(), Square::A1);

    app.dispatch(Command::FlipBoard);
    assert_eq!(app.orientation(), Orientation::Black);

    app.dispatch(Command::MoveCursor {
        file_delta: -1,
        rank_delta: -1,
    });
    assert_eq!(app.cursor(), Square::B2);

    app.dispatch(Command::MoveCursor {
        file_delta: 1,
        rank_delta: 1,
    });
    assert_eq!(app.cursor(), Square::A1);
}

#[test]
fn selection_rejects_enemy_and_illegal_destinations_but_allows_reselection() {
    let mut app = App::new();

    app.dispatch(Command::ActivateSquare(Square::E7));
    assert_eq!(app.selected(), None);
    assert!(app.legal_moves().is_empty());
    assert_eq!(app.message(), "Select a White piece");

    app.dispatch(Command::ActivateSquare(Square::A1));
    assert_eq!(app.selected(), Some(Square::A1));
    assert!(app.legal_moves().is_empty());
    assert_eq!(app.message(), "a1 has no legal moves");

    app.dispatch(Command::ActivateSquare(Square::E2));
    assert_eq!(app.selected(), Some(Square::E2));
    assert_eq!(app.legal_moves().len(), 2);
    assert!(app.legal_moves().contains(&"e2e3".parse().unwrap()));
    assert!(app.legal_moves().contains(&"e2e4".parse().unwrap()));

    app.dispatch(Command::ActivateSquare(Square::E5));
    assert_eq!(app.game().ply_count(), 0);
    assert_eq!(app.selected(), Some(Square::E2));
    assert_eq!(app.message(), "e2 cannot move to e5");

    app.dispatch(Command::ActivateSquare(Square::G1));
    assert_eq!(app.selected(), Some(Square::G1));
    assert_eq!(app.legal_moves().len(), 2);

    app.dispatch(Command::ActivateSquare(Square::G1));
    assert_eq!(app.selected(), None);
    assert!(app.legal_moves().is_empty());
    assert_eq!(app.message(), "White to move");
}

#[test]
fn activate_plays_a_selected_legal_move_and_clears_transient_state() {
    let mut app = App::new();

    assert_eq!(app.dispatch(Command::Activate), CommandResult::Continue);
    assert_eq!(app.selected(), Some(Square::E2));

    app.dispatch(Command::MoveCursor {
        file_delta: 0,
        rank_delta: 2,
    });
    assert_eq!(app.cursor(), Square::E4);
    assert_eq!(app.dispatch(Command::Activate), CommandResult::Continue);

    assert_eq!(app.game().side_to_move(), Color::Black);
    assert_eq!(app.game().ply_count(), 1);
    assert_eq!(app.game().piece_at(Square::E2), None);
    assert_eq!(app.game().piece_at(Square::E4).unwrap().piece, Piece::Pawn);
    assert_eq!(app.game().last_move().unwrap().san, "e4");
    assert_eq!(app.cursor(), Square::E4);
    assert_eq!(app.selected(), None);
    assert!(app.legal_moves().is_empty());
    assert_eq!(app.message(), "e4 played");
}

#[test]
fn undo_and_new_game_restore_play_state_without_resetting_view_preferences() {
    let mut app = App::new();
    let initial_fen = app.game().fen();

    app.dispatch(Command::Undo);
    assert_eq!(app.game().fen(), initial_fen);
    assert_eq!(app.message(), "Nothing to undo");

    dispatch_move(&mut app, "e2e4");
    app.dispatch(Command::ActivateSquare(Square::E7));
    assert_eq!(app.selected(), Some(Square::E7));

    app.dispatch(Command::Undo);
    assert_eq!(app.game().fen(), initial_fen);
    assert_eq!(app.game().side_to_move(), Color::White);
    assert_eq!(app.selected(), None);
    assert!(app.legal_moves().is_empty());
    assert_eq!(app.pending_promotion(), None);
    assert_eq!(app.cursor(), Square::E2);
    assert_eq!(app.message(), "Move undone — White to move");

    dispatch_move(&mut app, "e2e4");
    dispatch_move(&mut app, "e7e5");
    app.dispatch(Command::FlipBoard);
    app.dispatch(Command::ToggleCoordinates);
    assert_eq!(app.orientation(), Orientation::Black);
    assert!(!app.coordinates_visible());

    app.dispatch(Command::NewGame);
    assert_eq!(app.game().fen(), initial_fen);
    assert!(app.game().history().is_empty());
    assert_eq!(app.cursor(), Square::E2);
    assert_eq!(app.selected(), None);
    assert_eq!(app.orientation(), Orientation::Black);
    assert!(!app.coordinates_visible());
    assert_eq!(app.message(), "New game — White to move");
}

#[test]
fn help_coordinates_flip_cancel_and_quit_are_reduced_deterministically() {
    let mut app = App::new();
    assert!(!app.help_visible());
    assert!(app.coordinates_visible());
    assert_eq!(app.orientation(), Orientation::White);

    app.dispatch(Command::ToggleHelp);
    assert!(app.help_visible());
    let cursor_under_help = app.cursor();
    let orientation_under_help = app.orientation();
    let coordinates_under_help = app.coordinates_visible();
    app.dispatch(Command::ActivateSquare(Square::A8));
    assert_eq!(app.cursor(), cursor_under_help);
    assert_eq!(app.selected(), None);
    app.dispatch(Command::MoveCursor {
        file_delta: 1,
        rank_delta: 0,
    });
    app.dispatch(Command::FlipBoard);
    app.dispatch(Command::ToggleCoordinates);
    app.dispatch(Command::NewGame);
    app.dispatch(Command::Undo);
    assert_eq!(app.cursor(), cursor_under_help);
    assert_eq!(app.orientation(), orientation_under_help);
    assert_eq!(app.coordinates_visible(), coordinates_under_help);

    app.dispatch(Command::Cancel);
    assert!(!app.help_visible());

    app.dispatch(Command::ToggleCoordinates);
    assert!(!app.coordinates_visible());
    app.dispatch(Command::FlipBoard);
    assert_eq!(app.orientation(), Orientation::Black);

    assert_eq!(app.dispatch(Command::Quit), CommandResult::Quit);
}

#[test]
fn fools_mate_announces_checkmate_and_rejects_further_activation_until_undo() {
    let mut app = App::new();
    for notation in ["f2f3", "e7e5", "g2g4", "d8h4"] {
        dispatch_move(&mut app, notation);
    }

    assert_eq!(
        app.game().status(),
        GameStatus::Checkmate {
            winner: Color::Black,
        }
    );
    assert_eq!(app.game().history().len(), 4);
    assert_eq!(app.message(), "Qh4# — checkmate, Black wins");
    let terminal_fen = app.game().fen();

    app.dispatch(Command::ActivateSquare(Square::A2));
    assert_eq!(app.game().fen(), terminal_fen);
    assert_eq!(app.game().history().len(), 4);
    assert_eq!(app.selected(), None);
    assert!(app.legal_moves().is_empty());
    assert_eq!(app.message(), "Game over — press n for a new game");

    app.dispatch(Command::Undo);
    assert_eq!(app.game().status(), GameStatus::Ongoing);
    assert_eq!(app.game().side_to_move(), Color::Black);
    assert_eq!(app.game().history().len(), 3);
    assert_eq!(app.cursor(), Square::D8);
    assert_eq!(app.message(), "Move undone — Black to move");
}

#[test]
fn en_passant_is_exposed_as_a_selectable_capture_and_played_through_the_reducer() {
    let mut app = App::new();
    for notation in ["e2e4", "a7a6", "e4e5", "d7d5"] {
        dispatch_move(&mut app, notation);
    }
    assert_eq!(app.game().en_passant_target(), Some(Square::D6));

    app.dispatch(Command::ActivateSquare(Square::E5));
    let en_passant: ChessMove = "e5d6".parse().unwrap();
    assert_eq!(app.selected(), Some(Square::E5));
    assert!(app.legal_moves().contains(&en_passant));

    app.dispatch(Command::ActivateSquare(Square::D6));
    let record = app.game().last_move().unwrap();
    assert_eq!(record.mv, en_passant);
    assert_eq!(record.capture_square, Some(Square::D5));
    assert_eq!(record.captured.unwrap().color, Color::Black);
    assert_eq!(record.captured.unwrap().piece, Piece::Pawn);
    assert_eq!(app.game().piece_at(Square::D5), None);
    assert_eq!(app.game().piece_at(Square::D6).unwrap().color, Color::White);
    assert_eq!(app.game().piece_at(Square::D6).unwrap().piece, Piece::Pawn);
    assert_eq!(app.selected(), None);
    assert!(app.legal_moves().is_empty());
    assert_eq!(app.message(), "exd6 played");
}

#[test]
fn promotion_can_be_prompted_cancelled_and_completed_with_an_explicit_choice() {
    let mut app = App::new();
    for notation in [
        "a2a4", "h7h5", "a4a5", "h5h4", "a5a6", "h4h3", "a6b7", "h3g2",
    ] {
        dispatch_move(&mut app, notation);
    }

    app.dispatch(Command::ActivateSquare(Square::B7));
    assert_eq!(app.selected(), Some(Square::B7));
    for piece in [Piece::Queen, Piece::Rook, Piece::Bishop, Piece::Knight] {
        assert!(
            app.legal_moves()
                .contains(&ChessMove::new(Square::B7, Square::A8, Some(piece)))
        );
    }

    app.dispatch(Command::ActivateSquare(Square::A8));
    assert_eq!(
        app.pending_promotion(),
        Some(PendingPromotion {
            from: Square::B7,
            to: Square::A8,
        })
    );
    assert_eq!(app.game().ply_count(), 8);
    assert_eq!(app.message(), "Promote to queen, rook, bishop, or knight");

    app.dispatch(Command::ActivateSquare(Square::H8));
    assert_eq!(app.cursor(), Square::A8);
    assert_eq!(app.selected(), Some(Square::B7));
    assert_eq!(
        app.pending_promotion(),
        Some(PendingPromotion {
            from: Square::B7,
            to: Square::A8,
        })
    );
    let promotion_fen = app.game().fen();
    let promotion_orientation = app.orientation();
    let promotion_coordinates = app.coordinates_visible();
    app.dispatch(Command::NewGame);
    app.dispatch(Command::Undo);
    app.dispatch(Command::FlipBoard);
    app.dispatch(Command::ToggleCoordinates);
    app.dispatch(Command::ToggleHelp);
    assert_eq!(app.game().fen(), promotion_fen);
    assert_eq!(app.orientation(), promotion_orientation);
    assert_eq!(app.coordinates_visible(), promotion_coordinates);
    assert!(!app.help_visible());
    assert!(app.pending_promotion().is_some());

    app.dispatch(Command::Promote(Piece::Pawn));
    assert!(app.pending_promotion().is_some());
    assert_eq!(app.game().ply_count(), 8);
    assert_eq!(app.message(), "Choose queen, rook, bishop, or knight");

    app.dispatch(Command::Cancel);
    assert_eq!(app.pending_promotion(), None);
    assert_eq!(app.selected(), Some(Square::B7));
    assert_eq!(app.game().piece_at(Square::B7).unwrap().piece, Piece::Pawn);
    assert_eq!(app.game().piece_at(Square::A8).unwrap().piece, Piece::Rook);
    assert_eq!(app.message(), "Promotion cancelled");

    app.dispatch(Command::ActivateSquare(Square::A8));
    assert!(app.pending_promotion().is_some());
    app.dispatch(Command::Promote(Piece::Knight));

    let record = app.game().last_move().unwrap();
    assert_eq!(
        record.mv,
        ChessMove::new(Square::B7, Square::A8, Some(Piece::Knight))
    );
    assert_eq!(record.captured.unwrap().color, Color::Black);
    assert_eq!(record.captured.unwrap().piece, Piece::Rook);
    assert_eq!(app.game().piece_at(Square::B7), None);
    assert_eq!(app.game().piece_at(Square::A8).unwrap().color, Color::White);
    assert_eq!(
        app.game().piece_at(Square::A8).unwrap().piece,
        Piece::Knight
    );
    assert_eq!(app.game().side_to_move(), Color::Black);
    assert_eq!(app.pending_promotion(), None);
    assert_eq!(app.selected(), None);
    assert!(app.legal_moves().is_empty());
    assert_eq!(app.message(), "bxa8=N played");
}

#[test]
fn engine_games_enforce_turn_ownership_and_validate_engine_moves() {
    let mut app = App::vs_engine(Color::White);
    assert_eq!(app.engine_color(), Some(Color::Black));
    assert_eq!(app.orientation(), Orientation::White);
    assert!(app.is_human_turn());
    assert_eq!(app.position_revision(), 0);

    dispatch_move(&mut app, "e2e4");
    assert!(app.is_engine_turn());
    assert_eq!(app.position_revision(), 1);

    let reply: ChessMove = "e7e5".parse().unwrap();
    app.apply_engine_move(reply).unwrap();
    assert!(app.is_human_turn());
    assert_eq!(app.position_revision(), 2);
    assert_eq!(app.game().last_move().unwrap().mv, reply);

    assert_eq!(
        app.apply_engine_move("g1f3".parse().unwrap()),
        Err(EngineMoveError::NotEngineTurn)
    );
}

#[test]
fn engine_undo_returns_to_the_previous_human_decision() {
    let mut app = App::vs_engine(Color::White);
    dispatch_move(&mut app, "e2e4");
    app.apply_engine_move("e7e5".parse().unwrap()).unwrap();

    app.dispatch(Command::Undo);

    assert!(app.game().history().is_empty());
    assert_eq!(app.game().side_to_move(), Color::White);
    assert_eq!(app.cursor(), Square::E2);
    assert_eq!(app.position_revision(), 3);
    assert_eq!(app.message(), "Turn undone — White to move");
}

#[test]
fn engine_opening_as_white_cannot_be_undone_before_the_human_moves() {
    let mut app = App::vs_engine(Color::Black);
    assert_eq!(app.orientation(), Orientation::Black);
    assert!(app.is_engine_turn());
    app.apply_engine_move("e2e4".parse().unwrap()).unwrap();
    assert!(app.is_human_turn());

    app.dispatch(Command::Undo);

    assert_eq!(app.game().history().len(), 1);
    assert_eq!(app.game().last_move().unwrap().mv, "e2e4".parse().unwrap());
    assert_eq!(app.message(), "Nothing to undo yet");
}

#[test]
fn engine_failure_stays_visible_and_blocks_further_moves() {
    let mut app = App::vs_engine(Color::White);
    app.set_engine_failure("Engine failed: executable not found");
    app.set_engine_failure("Engine failed: supervisor disconnected");

    app.dispatch(Command::ActivateSquare(Square::E2));
    app.dispatch(Command::ActivateSquare(Square::E4));
    app.dispatch(Command::Cancel);

    assert!(app.is_human_turn());
    assert!(app.game().history().is_empty());
    assert_eq!(
        app.engine_failure(),
        Some("Engine failed: executable not found")
    );
    assert_eq!(app.message(), "Engine failed: executable not found");
}

#[test]
fn cancel_does_not_claim_the_human_can_move_during_engine_thinking() {
    let mut app = App::vs_engine(Color::White);
    app.set_engine_name("Maia-3");
    dispatch_move(&mut app, "e2e4");
    app.set_engine_status("Maia-3 is thinking");

    app.dispatch(Command::Cancel);

    assert!(app.is_engine_turn());
    assert_eq!(app.message(), "Maia-3 is thinking");
}

#[test]
fn managed_engine_setup_blocks_play_and_reduces_install_requests() {
    let mut app = App::vs_engine(Color::White);
    let original_cursor = app.cursor();
    let original_orientation = app.orientation();
    app.set_engine_setup_state(EngineSetupState::InstallPrompt);

    app.dispatch(Command::ActivateSquare(Square::E2));
    app.dispatch(Command::MoveCursor {
        file_delta: 1,
        rank_delta: 0,
    });
    app.dispatch(Command::FlipBoard);
    app.dispatch(Command::NewGame);
    assert_eq!(app.cursor(), original_cursor);
    assert_eq!(app.orientation(), original_orientation);
    assert!(app.game().history().is_empty());
    assert_eq!(
        app.dispatch(Command::InstallEngine),
        CommandResult::InstallEngineRequested
    );

    app.set_engine_setup_state(EngineSetupState::Installing {
        step: "Downloading dependencies".to_owned(),
    });
    assert_eq!(
        app.dispatch(Command::InstallEngine),
        CommandResult::Continue,
        "an active install cannot be started twice"
    );

    app.set_engine_setup_state(EngineSetupState::Failed {
        message: "network unavailable".to_owned(),
    });
    assert_eq!(
        app.dispatch(Command::InstallEngine),
        CommandResult::InstallEngineRequested
    );
    assert_eq!(app.dispatch(Command::Quit), CommandResult::Quit);

    app.clear_engine_setup_state();
    assert_eq!(app.engine_setup_state(), None);
    app.dispatch(Command::ActivateSquare(Square::E2));
    assert_eq!(app.selected(), Some(Square::E2));
}
