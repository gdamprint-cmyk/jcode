// reference: no prior art, searched the jcode source tree (crates/jcode-tui/src/tui/session_picker/)
// for an existing live-visibility check in this repo's own code; the loader
// (`load_sessions` / `load_sessions_inner`) and the picker filter
// (`filter.rs`: `show_test || !session.is_debug`) are the code under test.
//
// This is a LIVE diagnostic of THIS host's real ~/.jcode/sessions directory,
// so it is `#[ignore]`d by default and must be run explicitly:
//   cargo test -p jcode-tui --lib live_picker_lists_the_tv3_session -- --ignored --nocapture
use super::*;

const TV3_SESSION_ID: &str = "session_zebra_1794712810026_670cb896b3d06096";

#[test]
#[ignore = "live diagnostic of this host's session directory"]
fn live_picker_lists_the_tv3_session() {
    // The real home, not a temp dir: this is a diagnostic of the live host.
    let home = crate::storage::jcode_dir().expect("jcode home");
    let sessions_dir = home.join("sessions");
    assert!(
        sessions_dir.exists(),
        "live sessions dir missing: {}",
        sessions_dir.display()
    );

    // The session may not exist on another host: report and skip instead of
    // failing a shared test run.
    if !sessions_dir
        .join(format!("{TV3_SESSION_ID}.json"))
        .exists()
    {
        println!("live picker: {TV3_SESSION_ID} is absent on this host, skipping");
        return;
    }

    let sessions = super::super::loading::load_sessions().expect("load live sessions");

    let hit = sessions.iter().find(|s| s.id == TV3_SESSION_ID);
    println!(
        "live picker: total_sessions={} tv3_present={} tv3_is_debug={:?}",
        sessions.len(),
        hit.is_some(),
        hit.map(|s| s.is_debug),
    );
    let hit = hit.unwrap_or_else(|| {
        panic!(
            "TV-3 session {} is absent from the loaded session list (total {})",
            TV3_SESSION_ID,
            sessions.len()
        )
    });
    println!(
        "live picker: tv3 title={:?} updated={:?}",
        hit.title, hit.last_message_time
    );
    // The picker filter keeps a session only when `!is_debug` (unless the user
    // toggles test sessions on). So the visible condition is `is_debug == false`.
    assert!(
        !hit.is_debug,
        "TV-3 session is still hidden from the picker: is_debug=true"
    );
    println!("live picker: TV-3 session is VISIBLE (is_debug=false)");

    // The resume path must agree with the picker: `Session::load` applies the
    // journal metadata in order with the last entry winning
    // (session/persistence.rs:188 -> session.rs:774), so a resumed agent
    // inherits `is_debug=false` instead of re-forcing the old value.
    let loaded = crate::session::Session::load(TV3_SESSION_ID).expect("load TV-3 session");
    println!(
        "live resume: loaded.is_debug={} loaded.messages={} loaded.status={:?}",
        loaded.is_debug,
        loaded.messages.len(),
        loaded.status,
    );
    assert!(
        !loaded.is_debug,
        "resuming TV-3 would re-hide it: Session::load is_debug=true"
    );
    println!("live resume: TV-3 session resumes with is_debug=false");
}
