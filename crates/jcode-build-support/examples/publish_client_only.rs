//! Publish the current repo's self-dev build to the CLIENT channel only.
//!
//! Self-dev helper, 2026-11-04: `jcode debug reload` publishes the build AND
//! relaunches the shared server, which interrupts every running swarm member.
//! The voice-input fix (a client-side change) does not need a server restart,
//! so this publishes only the `current` channel and the client launcher,
//! leaving `shared-server` and the running daemon untouched.
//!
//! ```sh
//! cargo run -p jcode-build-support --example publish_client_only
//! ```
fn main() -> anyhow::Result<()> {
    let repo = std::env::current_dir()?;
    let state = jcode_build_support::current_source_state(&repo)?;
    let published = jcode_build_support::publish_local_current_build_for_source(&repo, &state)?;
    println!(
        "published {} to the client channel: {}",
        published.version,
        published.current_link.display()
    );
    println!("shared-server channel and the running daemon were NOT touched");
    Ok(())
}
