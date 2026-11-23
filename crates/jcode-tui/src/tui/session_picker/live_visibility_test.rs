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

/// The two other sessions an earlier draft of the research document named as
/// "affected". Both were reported as hidden, which a hand-rolled script got
/// wrong because it read the journal entries in reverse order. The production
/// loader and the production resume path are the decisive instruments here.
const ALSO_CHECKED_SESSION_IDS: &[&str] = &[
    "session_shrimp_1794689551271_157a06d4a4702a23",
    "session_sauropod_1794690463272_386ad27d1bb2fbcb",
];

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

/// Same instrument as above, applied to every session the research document
/// names as affected. A session counts as visible when the production loader
/// reports `is_debug == false`, and it must stay visible when resumed.
#[test]
#[ignore = "live diagnostic of this host's session directory"]
fn live_picker_lists_every_affected_session() {
    let home = crate::storage::jcode_dir().expect("jcode home");
    let sessions_dir = home.join("sessions");
    assert!(
        sessions_dir.exists(),
        "live sessions dir missing: {}",
        sessions_dir.display()
    );

    let sessions = super::super::loading::load_sessions().expect("load live sessions");
    println!("live picker (all): total_sessions={}", sessions.len());

    let mut ids: Vec<&str> = vec![TV3_SESSION_ID];
    ids.extend_from_slice(ALSO_CHECKED_SESSION_IDS);

    for id in ids {
        if !sessions_dir.join(format!("{id}.json")).exists() {
            println!("live picker (all): {id} is absent on this host, skipping");
            continue;
        }

        // The production loader: this is the value the picker filters on.
        let hit = sessions.iter().find(|s| s.id == id);
        let picker_is_debug = hit.map(|s| s.is_debug);
        println!(
            "live picker (all): {id} present={} is_debug={:?}",
            hit.is_some(),
            picker_is_debug,
        );

        // The production resume path: what an agent would inherit.
        let loaded = crate::session::Session::load(id).expect("load session");
        println!(
            "live resume (all): {id} is_debug={} messages={} status={:?}",
            loaded.is_debug,
            loaded.messages.len(),
            loaded.status,
        );

        assert!(
            picker_is_debug == Some(false),
            "{id} is hidden from the picker: loader is_debug={:?}",
            picker_is_debug,
        );
        assert!(
            !loaded.is_debug,
            "resuming {id} would re-hide it: Session::load is_debug=true",
        );
        println!("live picker (all): {id} is VISIBLE and resumes VISIBLE (is_debug=false)");
    }
}
