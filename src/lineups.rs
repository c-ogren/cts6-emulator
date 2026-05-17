//! Preset event lineups loaded by `/lineup preset <name>`.
//!
//! Each function returns a `Vec<EventDef>` that the command handler
//! assigns directly to `State::lineup`. Add new presets here, then
//! wire them into the `"preset"` arm of `handle_command` in
//! `main.rs` so they're reachable from the REPL.
use crate::{EventDef, Gender, Stroke};

/// Standard NFHS high-school dual-meet event order (12 events).
/// All events share the caller-supplied gender; in a real coed dual
/// the same 12 are run twice (girls then boys). Splits default to
/// 50 yd, which matches CTS6 split-arm behaviour for these races.
pub(crate) fn high_school_lineup_gender(gender: Gender) -> Vec<EventDef> {
    use Stroke::*;
    let mk = |distance: u16, stroke: Stroke| EventDef {
        distance,
        gender,
        stroke,
        split_yards: 50,
    };
    vec![
        mk(200, MedleyRelay), //  1. 200 medley relay   (BK/BR/FL/FR, 50 each)
        mk(200, Free),        //  2. 200 free
        mk(200, Im),          //  3. 200 IM             (FL/BK/BR/FR, 50 each)
        mk(50, Free),         //  4. 50 free
        EventDef {
            //  5. diving
            distance: 0,
            gender: gender,
            stroke: Diving,
            split_yards: 0,
        },
        mk(100, Fly),            //  6. 100 fly
        mk(100, Free),           //  7. 100 free
        mk(500, Free),           //  8. 500 free
        mk(200, FreestyleRelay), //  9. 200 free relay     (4×50 free)
        mk(100, Back),           // 10. 100 back
        mk(100, Breast),         // 11. 100 breast
        mk(400, FreestyleRelay), // 12. 400 free relay     (4×100 free, splits every 50)
    ]
}

/// Coed high-school dual-meet order: the standard 12-event NFHS
/// program run girls-then-boys for every event (diving stays mixed).
#[allow(dead_code)]
pub(crate) fn coed_high_school_lineup() -> Vec<EventDef> {
    use Gender::*;
    use Stroke::*;
    let mk = |distance: u16, stroke: Stroke, gender: Gender| EventDef {
        distance,
        gender,
        stroke,
        split_yards: 50,
    };
    vec![
        mk(200, MedleyRelay, Female), //  1. 200 medley relay   (BK/BR/FL/FR, 50 each)
        mk(200, MedleyRelay, Male),   //  2. 200 medley relay   (BK/BR/FL/FR, 50 each)
        mk(200, Free, Female),        //  3. 200 free
        mk(200, Free, Male),          //  4. 200 free
        mk(200, Im, Female),          //  5. 200 IM             (FL/BK/BR/FR, 50 each)
        mk(200, Im, Male),            //  6. 200 IM             (FL/BK/BR/FR, 50 each)
        mk(50, Free, Female),         //  7. 50 free
        mk(50, Free, Male),           //  8. 50 free
        EventDef {
            //  9. diving
            distance: 0,
            gender: Mixed,
            stroke: Diving,
            split_yards: 0,
        },
        mk(100, Fly, Female),            // 10. 100 fly
        mk(100, Fly, Male),              // 11. 100 fly
        mk(100, Free, Female),           // 12. 100 free
        mk(100, Free, Male),             // 13. 100 free
        mk(500, Free, Female),           // 14. 500 free
        mk(500, Free, Male),             // 15. 500 free
        mk(200, FreestyleRelay, Female), // 16. 200 free relay     (4×50 free)
        mk(200, FreestyleRelay, Male),   // 17. 200 free relay     (4×50 free)
        mk(100, Back, Female),           // 18. 100 back
        mk(100, Back, Male),             // 19. 100 back
        mk(100, Breast, Female),         // 20. 100 breast
        mk(100, Breast, Male),           // 21. 100 breast
        mk(400, FreestyleRelay, Female), // 22. 400 free relay     (4×100 free, splits every 50)
        mk(400, FreestyleRelay, Male),   // 23. 400 free relay     (4×100 free, splits every 50)
    ]
}

// ─── NCAA presets ──────────────────────────────────────────────────────
// Taken from the CTS 6 hardware's "NCAA 13-event", "NCAA 15-event", and "NCAA 16-event" lineups

pub(crate) fn ncaa_13_event(gender: Gender) -> Vec<EventDef> {
    use Stroke::*;
    let mk = |distance: u16, stroke: Stroke, gender: Gender| EventDef {
        distance,
        gender,
        stroke,
        split_yards: 50,
    };
    vec![
        mk(200, MedleyRelay, gender), //  1. 200 medley relay   (BK/BR/FL/FR, 50 each)
        mk(1000, Free, gender),       //  2. 1000 free
        mk(200, Free, gender),        //  3. 200 free
        mk(50, Free, gender),         //  4. 50 free
        mk(200, Im, gender),          //  5. 200 IM             (FL/BK/BR/FR, 50 each)
        EventDef {
            //  6. diving
            distance: 0,
            gender: gender,
            stroke: Diving,
            split_yards: 0,
        },
        mk(100, Fly, gender),  //  7. 100 fly
        mk(100, Free, gender), //  8. 100 free
        mk(100, Back, gender), //  9. 100 back
        mk(500, Free, gender), // 10. 500 free
        EventDef {
            // 11. diving
            distance: 0,
            gender: gender,
            stroke: Diving,
            split_yards: 0,
        },
        mk(100, Breast, gender),         // 12. 100 breast
        mk(200, FreestyleRelay, gender), // 13. 200 free relay     (4×50 free)
    ]
}

pub(crate) fn ncaa_15_event(gender: Gender) -> Vec<EventDef> {
    use Stroke::*;
    let mk = |distance: u16, stroke: Stroke, gender: Gender| EventDef {
        distance,
        gender,
        stroke,
        split_yards: 50,
    };
    vec![
        mk(100, Back, gender),
        mk(100, Breast, gender),
        mk(100, Fly, gender),
        mk(1000, Free, gender),
        mk(200, Free, gender),
        mk(50, Free, gender),
        mk(200, MedleyRelay, gender),
        EventDef {
            distance: 0,
            gender: gender,
            stroke: Diving,
            split_yards: 0,
        },
        mk(200, Fly, gender),
        mk(100, Free, gender),
        mk(200, Back, gender),
        mk(500, Free, gender),
        EventDef {
            distance: 0,
            gender: gender,
            stroke: Diving,
            split_yards: 0,
        },
        mk(200, Breast, gender),
        mk(200, FreestyleRelay, gender),
    ]
}

pub(crate) fn ncaa_16_event(gender: Gender) -> Vec<EventDef> {
    use Stroke::*;
    let mk = |distance: u16, stroke: Stroke, gender: Gender| EventDef {
        distance,
        gender,
        stroke,
        split_yards: 50,
    };
    vec![
        mk(200, MedleyRelay, gender),
        mk(1000, Free, gender),
        mk(200, Free, gender),
        mk(100, Back, gender),
        mk(100, Breast, gender),
        mk(200, Fly, gender),
        mk(50, Free, gender),
        EventDef {
            distance: 0,
            gender: gender,
            stroke: Diving,
            split_yards: 0,
        },
        mk(100, Free, gender),
        mk(200, Back, gender),
        mk(200, Breast, gender),
        mk(500, Free, gender),
        mk(100, Fly, gender),
        EventDef {
            distance: 0,
            gender: gender,
            stroke: Diving,
            split_yards: 0,
        },
        mk(200, Im, gender),
        mk(200, FreestyleRelay, gender),
    ]
}
