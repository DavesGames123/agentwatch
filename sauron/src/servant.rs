//! Which servant a session is, and what colour says so.
//!
//! WHY THE SESSION ID AND NOT A TABLE
//! ----------------------------------
//! The board and the workspace launcher both need to agree on a session's
//! colour, and they never speak to each other: the launcher opens panes and
//! exits, and the board is a separate process reading logs some time later. A
//! shared table would have to be written, locked, garbage-collected, and kept
//! true across two machines' worth of restarts.
//!
//! So there is no table. The colour is a pure function of the session id, which
//! both sides already hold -- the launcher because it resumed (or minted) it,
//! the board because it is the log's own name. They agree by construction, and
//! keep agreeing across restarts, because neither is remembering anything.
//!
//! That is also why `mint_session_id` exists. A *fresh* pane has no session
//! until the agent starts one, so its colour could not be known at launch --
//! unless sauron chooses the id itself and hands it over with `--session-id`.
//! Then a brand-new pane is the same colour on the board as it is on screen from
//! the first frame, with no correlation step anywhere.
//!
//! WHAT THE COLOUR IS NOT
//! ----------------------
//! It is not the status colour. `ui::color_of` answers "what state is this in" --
//! cyan for working, gold for untested -- and that is shared by every session in
//! that state and changes underneath one as it progresses. This answers the
//! different question "which one of them is this", so it must be stable for the
//! life of a session and distinct between neighbours. Both are drawn on the same
//! card and they do not collide: the status owns the glyph and the status word,
//! this owns the name.
//!
//! grep targets:
//!   fn color_for       -- session id -> its colour, on the board and the pane
//!   fn name_for        -- session id -> its servant name
//!   fn mint_session_id -- a UUID for a pane that has no session yet
//!   const PALETTE      -- the colours, chosen to be told apart at a glance

/// The servant colours, as `(r, g, b)`.
///
/// Ten, not more: past about a dozen, hues start being told apart only by
/// staring, which defeats the purpose. A workspace with more panes than this
/// repeats a colour, and the name is what separates those two.
///
/// Chosen to sit apart from each other *and* from `ui`'s status palette, so a
/// glance at a card never confuses "which servant" with "what state". Deliberately
/// no red (blocked), no gold (needs test) and no grey (clear).
pub const PALETTE: &[(u8, u8, u8)] = &[
    (94, 214, 200),  // teal
    (166, 150, 255), // violet
    (120, 190, 255), // sky
    (240, 150, 190), // rose
    (150, 220, 130), // leaf
    (255, 190, 120), // apricot
    (130, 210, 235), // ice
    (205, 165, 255), // orchid
    (170, 200, 255), // periwinkle
    (235, 175, 235), // lilac
];

/// The servant names, in palette order, so a name and a colour are two ways of
/// saying the same thing rather than two facts to keep in step.
pub const NAMES: &[&str] = &[
    "frodo", "sam", "merry", "pippin", "gandalf", "aragorn", "legolas", "gimli", "boromir",
    "bilbo",
];

/// Which slot a session falls in: a stable hash of its id, folded to the roster.
///
/// FNV-1a rather than the `sha2` already in the tree, because this runs per row
/// per frame and a cryptographic digest to pick one of ten buckets is a strange
/// way to spend a redraw. Nothing here is security-sensitive -- the worst a
/// collision does is give two panes the same colour, which the name still
/// separates.
fn slot(session_id: &str) -> usize {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in session_id.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    (hash % PALETTE.len() as u64) as usize
}

/// This session's colour, as `(r, g, b)`. The same answer on the board and in
/// the pane, because both compute it from the same id.
pub fn color_for(session_id: &str) -> (u8, u8, u8) {
    PALETTE[slot(session_id)]
}

/// This session's servant name.
pub fn name_for(session_id: &str) -> &'static str {
    NAMES[slot(session_id)]
}

/// A fresh UUID, so a pane with no session yet can still be given one.
///
/// Version 4 in shape but not in provenance: the bits come from the clock, the
/// pid and a per-process counter rather than from a CSPRNG, because sauron has
/// no random dependency and this needs to be unique, not unguessable. Two panes
/// opened in the same launch differ by the counter; two launches differ by the
/// clock; two machines differ by the pid. An adversary who can predict one has
/// already got the ability to read the log it names.
pub fn mint_session_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id() as u64;

    // Three independent sources, mixed so that a small change in any one of them
    // moves the whole output rather than one field of it.
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&(nanos ^ seq.rotate_left(32)).to_be_bytes());
    bytes[8..].copy_from_slice(&(pid.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ seq).to_be_bytes());

    // The two nibbles that make it a well-formed v4 UUID. Claude Code validates
    // the shape, so this is load-bearing rather than cosmetic.
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;

    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_two_rosters_are_the_same_length() {
        // A name and a colour are one fact indexed two ways. If these ever
        // diverge, `name_for` indexes past the end of one of them.
        assert_eq!(PALETTE.len(), NAMES.len());
    }

    #[test]
    fn a_session_keeps_its_colour_and_its_name() {
        // The whole premise: the launcher and the board compute this separately,
        // at different times, in different processes.
        let id = "18de2b59-e56a-4dba-a5bd-8273772dc124";
        assert_eq!(color_for(id), color_for(id));
        assert_eq!(name_for(id), name_for(id));
    }

    #[test]
    fn different_sessions_generally_get_different_colours() {
        // Ten buckets, so collisions are expected and fine -- but a hash that
        // sent everything to one bucket would pass every other test here while
        // making the feature useless. Assert the spread instead.
        let ids: Vec<String> = (0..200).map(|_| mint_session_id()).collect();
        let mut seen = std::collections::BTreeSet::new();
        for id in &ids {
            seen.insert(color_for(id));
        }
        assert_eq!(
            seen.len(),
            PALETTE.len(),
            "200 sessions should reach every colour; reached {}",
            seen.len()
        );
    }

    #[test]
    fn a_minted_id_is_a_well_formed_v4_uuid() {
        // Claude Code validates `--session-id`, so a malformed one is a pane that
        // refuses to start rather than a cosmetic slip.
        let id = mint_session_id();
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            vec![8, 4, 4, 4, 12],
            "{id}"
        );
        assert!(id.chars().all(|c| c.is_ascii_hexdigit() || c == '-'), "{id}");
        assert_eq!(parts[2].chars().next(), Some('4'), "version nibble: {id}");
        assert!(
            matches!(parts[3].chars().next(), Some('8' | '9' | 'a' | 'b')),
            "variant nibble: {id}"
        );
    }

    #[test]
    fn minted_ids_do_not_repeat_within_a_launch() {
        // Panes are opened in a tight loop, well inside one clock tick, so the
        // counter is the only thing separating them. This is the test that fails
        // if it is ever dropped from the mix.
        let ids: std::collections::BTreeSet<String> =
            (0..1000).map(|_| mint_session_id()).collect();
        assert_eq!(ids.len(), 1000);
    }
}
