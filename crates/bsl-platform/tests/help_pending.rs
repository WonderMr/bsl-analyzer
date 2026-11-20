//! A load announced before it starts holds every lookup until it installs: no lookup in
//! between fixes the library default, and a caller that must not wait can see the load is
//! still running. Runs in its own test binary because the snapshot is process-wide.

use std::sync::mpsc;
use std::time::Duration;

use bsl_platform::{
    active_platform_help_request, begin_platform_help_load, install_platform_help,
    platform_help_pending, wait_for_platform_help, InstallOutcome, PlatformData, PlatformHelp,
    PlatformHelpRequest,
};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/help/corpus.json");

/// Long enough for a lookup that does not wait to have answered many times over.
const SETTLE: Duration = Duration::from_millis(300);

#[test]
fn a_lookup_waits_for_an_announced_load_and_reads_what_it_installs() {
    assert!(active_platform_help_request().is_none(), "nothing may fix the snapshot early");
    assert!(!platform_help_pending(), "no load is announced yet");
    assert!(wait_for_platform_help(Some(Duration::ZERO)), "nothing to wait for");

    let load = begin_platform_help_load();
    assert!(platform_help_pending());
    assert!(!wait_for_platform_help(Some(SETTLE)), "the load has installed nothing yet");

    let (looked_up, answer) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let request = PlatformData::instance().help_request().clone();
        looked_up.send(request).unwrap();
    });
    assert!(
        answer.recv_timeout(SETTLE).is_err(),
        "a lookup during the load answered instead of waiting for it"
    );
    assert!(active_platform_help_request().is_none(), "the waiting lookup fixed a snapshot");

    let corpus = std::path::Path::new(FIXTURE);
    let help = PlatformHelp::from_corpus_file(corpus);
    assert!(help.origin.is_some(), "{:?}", help.missing_reason);
    assert_eq!(install_platform_help(help), Ok(InstallOutcome::Installed));
    let served = answer.recv_timeout(Duration::from_secs(30)).expect("the lookup was not woken");
    assert_eq!(served, PlatformHelpRequest::ExternalPath(corpus.to_path_buf()));
    reader.join().unwrap();

    assert!(!platform_help_pending(), "an installed snapshot ends the wait");
    assert!(wait_for_platform_help(None));
    drop(load);
    assert!(!platform_help_pending());
    assert_eq!(PlatformData::instance().help_request(), &served);
}
