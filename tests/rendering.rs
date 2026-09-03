use std::{
    collections::{HashMap, HashSet},
    hash::{DefaultHasher, Hash, Hasher},
};

use chess_tui::render::{
    BoardLayout, BoardScene, BoardTheme, BoardWidget, CellSize, LastMove, Orientation, PieceKind,
    RasterOptions, RasterPieceStyle, Rgb, Side, Square, TargetKind, ViewPiece, compose_board_rgba,
    raster_side_for_cells, render_unicode_board,
};
use image::{RgbaImage, imageops::FilterType};
use ratatui::{buffer::Buffer, layout::Rect, style::Color, widgets::Widget};

fn square(file: u8, rank: u8) -> Square {
    Square::new(file, rank).expect("test coordinates are on the board")
}

fn pixel_hash(image: &RgbaImage) -> u64 {
    let mut hasher = DefaultHasher::new();
    image.dimensions().hash(&mut hasher);
    image.as_raw().hash(&mut hasher);
    hasher.finish()
}

fn raster_options(side_px: u32) -> RasterOptions {
    RasterOptions {
        side_px,
        show_coordinates: false,
        ..RasterOptions::default()
    }
}

fn pixel_art_options(side_px: u32) -> RasterOptions {
    RasterOptions {
        side_px,
        piece_style: RasterPieceStyle::PixelArt,
        show_coordinates: false,
        ..RasterOptions::default()
    }
}

#[test]
fn orientation_maps_every_square_round_trip() {
    for orientation in [Orientation::White, Orientation::Black] {
        for index in 0..64 {
            let square = Square::from_index(index).unwrap();
            let (column, row) = orientation.square_to_view(square);

            assert!(column < 8);
            assert!(row < 8);
            assert_eq!(orientation.view_to_square(column, row), Some(square));
        }

        for coordinate in 0..=u8::MAX {
            if coordinate >= 8 {
                assert_eq!(orientation.view_to_square(coordinate, 0), None);
                assert_eq!(orientation.view_to_square(0, coordinate), None);
            }
        }

        assert_eq!(orientation.flipped().flipped(), orientation);
    }

    assert_eq!(Orientation::White.square_to_view(square(0, 7)), (0, 0));
    assert_eq!(Orientation::White.square_to_view(square(7, 0)), (7, 7));
    assert_eq!(Orientation::Black.square_to_view(square(7, 0)), (0, 0));
    assert_eq!(Orientation::Black.square_to_view(square(0, 7)), (7, 7));
}

#[test]
fn board_layout_fits_centered_physically_square_cells() {
    let exact = BoardLayout::fit(Rect::new(3, 5, 83, 43), CellSize::new(8, 16));
    assert_eq!(exact.square_columns, 10);
    assert_eq!(exact.square_rows, 5);
    assert_eq!(exact.square_pixels(), (80, 80));
    assert_eq!(exact.board, Rect::new(4, 6, 80, 40));
    assert!(!exact.is_empty());
    assert_eq!(raster_side_for_cells(exact), 640);

    // Six columns by three rows are physically 48x48 pixels per square. The
    // fitter therefore leaves horizontal room unused instead of stretching.
    let letterboxed = BoardLayout::fit(Rect::new(0, 0, 64, 24), CellSize::new(8, 16));
    assert_eq!(letterboxed.square_columns, 6);
    assert_eq!(letterboxed.square_rows, 3);
    assert_eq!(letterboxed.square_pixels(), (48, 48));
    assert_eq!(letterboxed.board, Rect::new(8, 0, 48, 24));

    let zero_sized_cells = BoardLayout::fit(Rect::new(0, 0, 8, 8), CellSize::new(0, 0));
    assert_eq!(zero_sized_cells.cell_size, CellSize::new(1, 1));
    assert_eq!(zero_sized_cells.square_pixels(), (1, 1));
}

#[test]
fn hit_testing_obeys_every_square_and_board_boundary() {
    let layout = BoardLayout::fit(Rect::new(3, 5, 83, 43), CellSize::new(8, 16));

    for orientation in [Orientation::White, Orientation::Black] {
        for index in 0..64 {
            let square = Square::from_index(index).unwrap();
            let rect = layout.square_rect(square, orientation).unwrap();

            assert_eq!(layout.square_at(rect.x, rect.y, orientation), Some(square));
            assert_eq!(
                layout.square_at(
                    rect.x + rect.width - 1,
                    rect.y + rect.height - 1,
                    orientation
                ),
                Some(square)
            );
        }

        assert_eq!(
            layout.square_at(layout.board.x - 1, layout.board.y, orientation),
            None
        );
        assert_eq!(
            layout.square_at(layout.board.x, layout.board.y - 1, orientation),
            None
        );
        assert_eq!(
            layout.square_at(
                layout.board.x + layout.board.width,
                layout.board.y,
                orientation
            ),
            None
        );
        assert_eq!(
            layout.square_at(
                layout.board.x,
                layout.board.y + layout.board.height,
                orientation
            ),
            None
        );
    }

    let top_left = layout.board;
    assert_eq!(
        layout.square_at(
            top_left.x + layout.square_columns - 1,
            top_left.y,
            Orientation::White
        ),
        Some(square(0, 7))
    );
    assert_eq!(
        layout.square_at(
            top_left.x + layout.square_columns,
            top_left.y,
            Orientation::White
        ),
        Some(square(1, 7))
    );
}

#[test]
fn tiny_and_coordinate_clipped_areas_are_safe() {
    for area in [Rect::new(11, 7, 7, 100), Rect::new(11, 7, 100, 7)] {
        let layout = BoardLayout::fit(area, CellSize::default());
        assert!(layout.is_empty());
        assert_eq!(layout.board, Rect::new(area.x, area.y, 0, 0));
        assert_eq!(layout.square_rect(square(0, 0), Orientation::White), None);
        assert_eq!(layout.square_at(area.x, area.y, Orientation::White), None);
        assert_eq!(raster_side_for_cells(layout), 8);

        let mut buffer = Buffer::empty(area);
        let untouched = buffer.clone();
        BoardWidget::new(&BoardScene::starting_position()).render(area, &mut buffer);
        assert_eq!(buffer, untouched);
    }

    // Rect::new clamps these requested dimensions at the u16 coordinate edge.
    let clipped = Rect::new(u16::MAX - 15, u16::MAX - 15, 100, 100);
    assert_eq!(clipped.width, 15);
    assert_eq!(clipped.height, 15);
    let scene = BoardScene::starting_position();
    let widget = BoardWidget::new(&scene).cell_size(CellSize::new(1, 1));
    let layout = widget.layout(clipped);
    assert_eq!(layout.board.width, 8);
    assert_eq!(layout.board.height, 8);
    assert!(layout.board.x >= clipped.x);
    assert!(layout.board.y >= clipped.y);
    assert!(layout.board.x + layout.board.width <= clipped.x + clipped.width);
    assert!(layout.board.y + layout.board.height <= clipped.y + clipped.height);

    let mut buffer = Buffer::empty(clipped);
    widget.render(clipped, &mut buffer);
    assert_eq!(buffer[(layout.board.x, layout.board.y)].symbol(), "♜");
}

#[test]
fn rgba_composition_normalizes_dimensions_and_is_byte_deterministic() {
    for (requested, normalized) in [
        (0, 8),
        (7, 8),
        (8, 8),
        (9, 8),
        (63, 56),
        (64, 64),
        (67, 64),
        (4_095, 4_088),
        (4_096, 4_096),
        (4_097, 4_096),
    ] {
        assert_eq!(
            RasterOptions::new(requested).normalized_side_px(),
            normalized
        );
    }

    let mut scene = BoardScene::starting_position();
    scene.cursor = Some(square(4, 3));
    scene.selected = Some(square(4, 1));
    scene.set_target(square(4, 2), Some(TargetKind::Quiet));
    scene.last_move = Some(LastMove::new(square(6, 0), square(5, 2)));
    let options = RasterOptions::new(131);

    let first = compose_board_rgba(&scene, &options);
    let second = compose_board_rgba(&scene.clone(), &options);
    assert_eq!(first.dimensions(), (128, 128));
    assert_eq!(first.as_raw().len(), 128 * 128 * 4);
    assert!(first.pixels().all(|pixel| pixel[3] == 255));
    assert_eq!(pixel_hash(&first), pixel_hash(&second));
    assert!(
        first.as_raw() == second.as_raw(),
        "equal scenes produced different RGBA bytes"
    );

    let minimum = compose_board_rgba(&BoardScene::empty(), &RasterOptions::new(0));
    assert_eq!(minimum.dimensions(), (8, 8));
    assert_eq!(minimum.as_raw().len(), 8 * 8 * 4);

    let mut empty = BoardScene::empty();
    empty.show_coordinates = false;
    let checker = compose_board_rgba(&empty, &raster_options(128));
    let a8_center = checker.get_pixel(8, 8);
    let b8_center = checker.get_pixel(24, 8);
    assert_ne!(
        a8_center, b8_center,
        "light and dark squares rendered identically"
    );
}

#[test]
fn every_piece_kind_has_a_distinct_non_background_silhouette() {
    let kinds = [
        PieceKind::Pawn,
        PieceKind::Knight,
        PieceKind::Bishop,
        PieceKind::Rook,
        PieceKind::Queen,
        PieceKind::King,
    ];
    let position = square(3, 3);
    let options = raster_options(256);
    let mut empty = BoardScene::empty();
    empty.show_coordinates = false;
    let background = compose_board_rgba(&empty, &options);
    let square_px = background.width() / 8;
    let (column, row) = empty.orientation.square_to_view(position);
    let x_start = u32::from(column) * square_px;
    let y_start = u32::from(row) * square_px;
    let mut masks = HashMap::new();

    for kind in kinds {
        let mut scene = empty.clone();
        scene.set_piece(position, Some(ViewPiece::new(Side::White, kind)));
        let image = compose_board_rgba(&scene, &options);
        let mut changed = Vec::with_capacity((square_px * square_px) as usize);

        for y in y_start..y_start + square_px {
            for x in x_start..x_start + square_px {
                changed.push(image.get_pixel(x, y) != background.get_pixel(x, y));
            }
        }

        let changed_count = changed.iter().filter(|changed| **changed).count();
        assert!(
            changed_count > 32,
            "{kind:?} did not produce a meaningful non-background silhouette"
        );
        masks.insert(kind, changed);
    }

    let unique_masks = masks.values().collect::<HashSet<_>>();
    assert_eq!(
        unique_masks.len(),
        kinds.len(),
        "two or more piece kinds produced the same silhouette"
    );
}

#[test]
fn every_piece_remains_distinct_at_six_sample_halfblock_resolution() {
    let kinds = [
        PieceKind::Pawn,
        PieceKind::Knight,
        PieceKind::Bishop,
        PieceKind::Rook,
        PieceKind::Queen,
        PieceKind::King,
    ];
    let position = square(4, 3);
    let options = pixel_art_options(48);
    let mut empty = BoardScene::empty();
    empty.show_coordinates = false;
    let background = compose_board_rgba(&empty, &options);
    let square_px = background.width() / 8;
    assert_eq!(square_px, 6, "test must exercise the 6x6 sprite path");
    let (column, row) = empty.orientation.square_to_view(position);
    let x_start = u32::from(column) * square_px;
    let y_start = u32::from(row) * square_px;

    for side in [Side::White, Side::Black] {
        let mut masks = HashMap::new();
        for kind in kinds {
            let mut scene = empty.clone();
            scene.set_piece(position, Some(ViewPiece::new(side, kind)));
            let image = compose_board_rgba(&scene, &options);
            let mut changed = Vec::with_capacity((square_px * square_px) as usize);

            for y in y_start..y_start + square_px {
                for x in x_start..x_start + square_px {
                    changed.push(image.get_pixel(x, y) != background.get_pixel(x, y));
                }
            }

            assert!(
                changed.iter().filter(|changed| **changed).count() >= 16,
                "{side:?} {kind:?} vanished at half-block resolution"
            );
            let base_row = square_px as usize * (square_px as usize - 1);
            assert!(
                !changed[base_row] && !changed[base_row + square_px as usize - 1],
                "{side:?} {kind:?} base touches a neighbor instead of keeping side gutters"
            );
            masks.insert(kind, changed);
        }

        assert_eq!(
            masks.values().collect::<HashSet<_>>().len(),
            kinds.len(),
            "two or more {side:?} pieces collapse to the same 6x6 silhouette"
        );
    }
}

#[test]
fn pixel_art_survives_halfblock_downsampling_from_a_large_source() {
    let kinds = [
        PieceKind::Pawn,
        PieceKind::Knight,
        PieceKind::Bishop,
        PieceKind::Rook,
        PieceKind::Queen,
        PieceKind::King,
    ];
    let position = square(4, 3);
    let source_options = pixel_art_options(320);
    let mut empty = BoardScene::empty();
    empty.show_coordinates = false;
    let background_source = compose_board_rgba(&empty, &source_options);
    let source_square_px = background_source.width() / 8;
    let (source_column, source_row) = empty.orientation.square_to_view(position);
    let source_x = u32::from(source_column) * source_square_px;
    let source_y = u32::from(source_row) * source_square_px;
    assert_eq!(
        background_source.get_pixel(source_x + 3, source_y + 3),
        background_source.get_pixel(
            source_x + source_square_px - 4,
            source_y + source_square_px - 4,
        ),
        "pixel-art squares should use a flat fill"
    );
    assert_eq!(
        background_source.get_pixel(0, 0),
        background_source.get_pixel(3, 3),
        "pixel-art frames should not add a board bevel"
    );
    let background = image::imageops::resize(&background_source, 48, 48, FilterType::Triangle);
    let square_px = background.width() / 8;
    assert_eq!(square_px, 6, "sample must match a 48x24 half-block board");
    let (column, row) = empty.orientation.square_to_view(position);
    let x_start = u32::from(column) * square_px;
    let y_start = u32::from(row) * square_px;

    for side in [Side::White, Side::Black] {
        let mut sampled_pieces = HashMap::new();
        for kind in kinds {
            let mut scene = empty.clone();
            scene.set_piece(position, Some(ViewPiece::new(side, kind)));
            let source = compose_board_rgba(&scene, &source_options);
            let sampled = image::imageops::resize(&source, 48, 48, FilterType::Triangle);
            let mut pixels = Vec::with_capacity((square_px * square_px) as usize);
            let mut clearly_changed = 0;

            for y in y_start..y_start + square_px {
                for x in x_start..x_start + square_px {
                    let pixel = sampled.get_pixel(x, y).0;
                    let base = background.get_pixel(x, y).0;
                    let difference = pixel[..3]
                        .iter()
                        .zip(&base[..3])
                        .map(|(channel, base)| channel.abs_diff(*base) as u16)
                        .sum::<u16>();
                    clearly_changed += usize::from(difference >= 24);
                    pixels.push(pixel);
                }
            }

            assert!(
                clearly_changed >= 14,
                "{side:?} {kind:?} vanished after half-block downsampling"
            );
            sampled_pieces.insert(kind, pixels);
        }

        assert_eq!(
            sampled_pieces.values().collect::<HashSet<_>>().len(),
            kinds.len(),
            "two or more {side:?} pixel-art pieces collapse after half-block downsampling"
        );
    }

    let mut comparison = empty;
    comparison.set_piece(
        position,
        Some(ViewPiece::new(Side::White, PieceKind::Knight)),
    );
    let pixel_art = compose_board_rgba(&comparison, &source_options);
    let detailed = compose_board_rgba(&comparison, &raster_options(320));
    assert_ne!(
        pixel_hash(&pixel_art),
        pixel_hash(&detailed),
        "explicit pixel-art and detailed styles unexpectedly rendered alike"
    );
}

#[test]
fn each_overlay_changes_the_rgba_frame_in_a_distinct_way() {
    let focus = square(4, 3);
    let mut baseline_scene = BoardScene::empty();
    baseline_scene.show_coordinates = false;
    let options = RasterOptions {
        side_px: 256,
        show_coordinates: true,
        ..RasterOptions::default()
    };
    let baseline = compose_board_rgba(&baseline_scene, &options);

    let mut cursor = baseline_scene.clone();
    cursor.cursor = Some(focus);
    let mut selected = baseline_scene.clone();
    selected.selected = Some(focus);
    let mut quiet = baseline_scene.clone();
    quiet.set_target(focus, Some(TargetKind::Quiet));
    let mut capture = baseline_scene.clone();
    capture.set_target(focus, Some(TargetKind::Capture));
    let mut checked = baseline_scene.clone();
    checked.checked = Some(focus);
    let mut last_move = baseline_scene.clone();
    last_move.last_move = Some(LastMove::new(square(4, 1), focus));
    let mut coordinates = baseline_scene.clone();
    coordinates.show_coordinates = true;

    let overlays = [
        ("cursor", cursor),
        ("selection", selected),
        ("quiet target", quiet),
        ("capture target", capture),
        ("check", checked),
        ("last move", last_move),
        ("coordinates", coordinates),
    ];
    let mut frame_hashes = HashSet::from([pixel_hash(&baseline)]);

    for (name, scene) in overlays {
        let frame = compose_board_rgba(&scene, &options);
        assert!(
            frame.as_raw() != baseline.as_raw(),
            "{name} overlay did not change any RGBA bytes"
        );
        assert!(
            frame_hashes.insert(pixel_hash(&frame)),
            "{name} overlay was pixel-identical to another tested frame"
        );
    }
}

#[test]
fn unicode_board_widget_renders_pieces_targets_coordinates_and_colors() {
    let mut scene = BoardScene::starting_position();
    scene.set_target(square(4, 3), Some(TargetKind::Quiet));
    let theme = BoardTheme::default();
    let area = Rect::new(5, 3, 44, 26);
    let widget = BoardWidget::new(&scene)
        .theme(theme)
        .cell_size(CellSize::new(8, 12));
    let layout = widget.layout(area);
    assert_eq!(layout.board, Rect::new(7, 4, 40, 24));
    assert_eq!(layout.square_columns, 5);
    assert_eq!(layout.square_rows, 3);

    let mut buffer = Buffer::empty(area);
    widget.render(area, &mut buffer);

    let a8 = layout
        .square_rect(square(0, 7), Orientation::White)
        .unwrap();
    let e8 = layout
        .square_rect(square(4, 7), Orientation::White)
        .unwrap();
    let a1 = layout
        .square_rect(square(0, 0), Orientation::White)
        .unwrap();
    let e1 = layout
        .square_rect(square(4, 0), Orientation::White)
        .unwrap();
    let e4 = layout
        .square_rect(square(4, 3), Orientation::White)
        .unwrap();

    assert_eq!(buffer[(a8.x, a8.y)].symbol(), "8");
    assert_eq!(buffer[(a8.x + 2, a8.y + 1)].symbol(), "♜");
    assert_eq!(buffer[(e8.x + 2, e8.y + 1)].symbol(), "♚");
    assert_eq!(buffer[(a1.x, a1.y)].symbol(), "1");
    assert_eq!(buffer[(a1.x + 2, a1.y + 1)].symbol(), "♜");
    assert_eq!(buffer[(a1.x + 4, a1.y + 2)].symbol(), "a");
    assert_eq!(buffer[(e1.x + 2, e1.y + 1)].symbol(), "♚");
    assert_eq!(buffer[(e4.x + 2, e4.y + 1)].symbol(), "•");

    assert_eq!(
        buffer[(a8.x + 2, a8.y + 1)].fg,
        Color::Rgb(
            theme.black_piece.red,
            theme.black_piece.green,
            theme.black_piece.blue
        )
    );
    assert_eq!(
        buffer[(a1.x + 2, a1.y + 1)].fg,
        Color::Rgb(
            theme.white_piece.red,
            theme.white_piece.green,
            theme.white_piece.blue
        )
    );
    assert_eq!(
        buffer[(a8.x + 2, a8.y + 1)].bg,
        Color::Rgb(
            theme.light_square.red,
            theme.light_square.green,
            theme.light_square.blue
        )
    );
    let b8 = layout
        .square_rect(square(1, 7), Orientation::White)
        .unwrap();
    assert_eq!(
        buffer[(b8.x + 2, b8.y + 1)].bg,
        Color::Rgb(
            theme.dark_square.red,
            theme.dark_square.green,
            theme.dark_square.blue
        )
    );
    assert_eq!(
        buffer[(e4.x + 2, e4.y + 1)].fg,
        Color::Rgb(
            theme.legal_target.red,
            theme.legal_target.green,
            theme.legal_target.blue
        )
    );
}

#[test]
fn unicode_board_honors_black_orientation_and_coordinate_toggle() {
    let mut scene = BoardScene::starting_position();
    scene.orientation = Orientation::Black;
    scene.show_coordinates = false;
    let area = Rect::new(0, 0, 8, 8);
    let mut buffer = Buffer::empty(area);

    BoardWidget::new(&scene)
        .cell_size(CellSize::new(1, 1))
        .render(area, &mut buffer);

    assert_eq!(buffer[(0, 0)].symbol(), "♜");
    assert_eq!(buffer[(4, 0)].symbol(), "♛");
    assert_eq!(buffer[(0, 7)].symbol(), "♜");
    assert_eq!(buffer[(4, 7)].symbol(), "♛");
    assert!(!buffer.content().iter().any(|cell| {
        matches!(
            cell.symbol(),
            "a" | "b"
                | "c"
                | "d"
                | "e"
                | "f"
                | "g"
                | "h"
                | "1"
                | "2"
                | "3"
                | "4"
                | "5"
                | "6"
                | "7"
                | "8"
        )
    }));
}

#[test]
fn roomy_unicode_squares_use_multi_cell_piece_sprites() {
    let piece_square = square(3, 3);
    let mut scene = BoardScene::empty();
    scene.show_coordinates = false;
    scene.set_piece(
        piece_square,
        Some(ViewPiece::new(Side::White, PieceKind::Queen)),
    );
    let layout = BoardLayout {
        board: Rect::new(0, 0, 48, 24),
        square_columns: 6,
        square_rows: 3,
        cell_size: CellSize::new(10, 20),
    };
    let mut buffer = Buffer::empty(layout.board);

    render_unicode_board(&scene, BoardTheme::default(), layout, &mut buffer);

    let piece_area = layout
        .square_rect(piece_square, Orientation::White)
        .expect("non-empty board has a piece square");
    let positions = || {
        (piece_area.y..piece_area.y + piece_area.height)
            .flat_map(|y| (piece_area.x..piece_area.x + piece_area.width).map(move |x| (x, y)))
    };
    let painted_cells = positions()
        .filter(|position| matches!(buffer[*position].symbol(), "▀" | "▄" | "█"))
        .count();
    assert!(painted_cells >= 8, "piece sprite rendered too sparsely");
    assert!(!positions().any(|position| buffer[position].symbol() == "♛"));
}

#[test]
fn even_width_unicode_squares_center_quiet_targets_between_cells() {
    let target_square = square(3, 3);
    let mut scene = BoardScene::empty();
    scene.show_coordinates = false;
    scene.set_target(target_square, Some(TargetKind::Quiet));
    let theme = BoardTheme::default();
    let layout = BoardLayout {
        board: Rect::new(0, 0, 48, 24),
        square_columns: 6,
        square_rows: 3,
        cell_size: CellSize::new(10, 20),
    };
    let mut buffer = Buffer::empty(layout.board);

    render_unicode_board(&scene, theme, layout, &mut buffer);

    let area = layout
        .square_rect(target_square, Orientation::White)
        .expect("non-empty board has a target square");
    let marker_y = area.y + area.height / 2;
    let marker_left = area.x + area.width / 2 - 1;
    let marker_right = marker_left + 1;
    let marker_color = Color::Rgb(
        theme.legal_target.red,
        theme.legal_target.green,
        theme.legal_target.blue,
    );
    assert_eq!(buffer[(marker_left, marker_y)].symbol(), "◗");
    assert_eq!(buffer[(marker_right, marker_y)].symbol(), "◖");
    assert_eq!(buffer[(marker_left, marker_y)].fg, marker_color);
    assert_eq!(buffer[(marker_right, marker_y)].fg, marker_color);
}

#[test]
fn compact_unicode_board_uses_cursor_rails_and_hides_cramped_coordinates() {
    let cursor = square(4, 1);
    let mut scene = BoardScene::starting_position();
    scene.cursor = Some(cursor);
    scene.show_coordinates = true;
    let theme = BoardTheme::default();
    let layout = BoardLayout {
        board: Rect::new(0, 0, 48, 16),
        square_columns: 6,
        square_rows: 2,
        cell_size: CellSize::new(10, 20),
    };
    let mut buffer = Buffer::empty(layout.board);

    render_unicode_board(&scene, theme, layout, &mut buffer);

    let cursor_area = layout
        .square_rect(cursor, Orientation::White)
        .expect("non-empty board has a cursor square");
    let cursor_color = Color::Rgb(theme.cursor.red, theme.cursor.green, theme.cursor.blue);
    assert_eq!(buffer[(cursor_area.x, cursor_area.y)].bg, cursor_color);
    assert_eq!(
        buffer[(cursor_area.x + cursor_area.width - 1, cursor_area.y + 1)].bg,
        cursor_color
    );
    assert_ne!(
        buffer[(cursor_area.x + cursor_area.width / 2, cursor_area.y)].bg,
        cursor_color,
        "a two-row cursor must not fill the entire square"
    );
    assert!(!buffer.content().iter().any(|cell| {
        matches!(
            cell.symbol(),
            "a" | "b"
                | "c"
                | "d"
                | "e"
                | "f"
                | "g"
                | "h"
                | "1"
                | "2"
                | "3"
                | "4"
                | "5"
                | "6"
                | "7"
                | "8"
        )
    }));
}

#[test]
fn custom_theme_is_reflected_by_both_renderers() {
    let theme = BoardTheme {
        light_square: Rgb::new(1, 2, 3),
        dark_square: Rgb::new(4, 5, 6),
        ..BoardTheme::default()
    };
    let mut scene = BoardScene::empty();
    scene.show_coordinates = false;

    let image = compose_board_rgba(
        &scene,
        &RasterOptions {
            side_px: 128,
            theme,
            show_coordinates: false,
            piece_style: RasterPieceStyle::Detailed,
        },
    );
    assert_ne!(image.get_pixel(8, 8), image.get_pixel(24, 8));

    let area = Rect::new(0, 0, 8, 8);
    let mut buffer = Buffer::empty(area);
    BoardWidget::new(&scene)
        .theme(theme)
        .cell_size(CellSize::new(1, 1))
        .render(area, &mut buffer);
    assert_eq!(buffer[(0, 0)].bg, Color::Rgb(1, 2, 3));
    assert_eq!(buffer[(1, 0)].bg, Color::Rgb(4, 5, 6));
}
