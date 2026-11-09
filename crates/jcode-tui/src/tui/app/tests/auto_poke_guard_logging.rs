// reference: no prior art, searched crates/jcode-tui/src/tui/app/tests for an
// existing auto-poke log assertion; the closest prior art is
// crates/jcode-tui/src/tui/app/tests/auto_poke_cycles.rs (state assertions,
// no log assertions), so this file follows that fixture style.
//
// The auto-poke guard in `schedule_auto_poke_followup_after_response` runs
// before every other AUTO_POKE_DECISION log site. Before the fix it returned
// silently, so a poke that was disabled by a stuck queue left NO log line at
// all and "poke is on but never fires" could not be diagnosed from the log.
//
// The guard must therefore name the cause it disabled on. This test drives
// every arm of the guard and asserts each cause is visible in the real log
// file the production logger writes.

#[test]
fn auto_poke_guard_logs_the_reason_it_disabled() {
    with_temp_jcode_home(|| {
        crate::logging::init();
        let logs_dir = crate::storage::logs_dir().expect("logs dir");

        let mut app = create_test_app();
        // Keep the default-on re-arm out of the way so each case exercises
        // exactly one arm of the guard.
        app.auto_poke_default_on = false;

        // 1. Auto-poke is not armed at all.
        app.auto_poke_incomplete_todos = false;
        app.pending_queued_dispatch = false;
        app.pending_turn = false;
        app.queued_messages.clear();
        assert!(!app.schedule_auto_poke_followup_if_needed());

        // 2. A queued dispatch has not gone out yet.
        app.auto_poke_incomplete_todos = true;
        app.pending_queued_dispatch = true;
        assert!(!app.schedule_auto_poke_followup_if_needed());
        app.pending_queued_dispatch = false;

        // 3. A turn is still running.
        app.pending_turn = true;
        assert!(!app.schedule_auto_poke_followup_if_needed());
        app.pending_turn = false;

        // 4. A follow-up is queued behind the current turn.
        app.queued_messages.push("queued follow-up".to_string());
        assert!(!app.schedule_auto_poke_followup_if_needed());
        app.queued_messages.clear();

        // Marker proves we are reading the file the logger actually writes to,
        // so a missing AUTO_POKE_DECISION line means "not logged", not
        // "read the wrong file".
        let marker = format!("AUTO_POKE_LOG_TEST_MARKER_{}", std::process::id());
        crate::logging::info(&marker);
        // Read every daily log the logger could have written: the file name
        // carries the date, so a run that crosses midnight would otherwise
        // split the evidence across two files.
        let mut log = String::new();
        let mut files: Vec<_> = std::fs::read_dir(&logs_dir)
            .unwrap_or_else(|err| panic!("read logs dir {}: {err}", logs_dir.display()))
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("jcode-") && name.ends_with(".log"))
            })
            .collect();
        files.sort();
        for path in files {
            if let Ok(text) = std::fs::read_to_string(&path) {
                log.push_str(&text);
            }
        }
        assert!(
            log.contains(&marker),
            "the logger must write into {} (marker missing), otherwise this test would \
             read the wrong file",
            logs_dir.display()
        );

        for reason in [
            "guard_not_armed",
            "guard_pending_queued_dispatch",
            "guard_pending_turn",
            "guard_queued_followups",
        ] {
            assert!(
                log.contains(&format!("AUTO_POKE_DECISION action=idle reason={reason}")),
                "the auto-poke guard must log why it disabled the poke; \
                 `{reason}` is missing from {}",
                logs_dir.display()
            );
        }
    });
}
