//! A load whose thread panics before it installs anything does not leave its waiting
//! lookups to the library default: they wake to the built-in facts, which say why they
//! serve, under a source that a configured one differs from. Runs in its own test binary
//! because the snapshot is process-wide.

use std::sync::mpsc;
use std::time::Duration;

use bsl_platform::{
    active_platform_help_request, begin_platform_help_load, install_platform_help,
    platform_help_pending, PlatformData, PlatformHelp, PlatformHelpRequest,
};

/// Long enough for a lookup that does not wait to have answered many times over.
const SETTLE: Duration = Duration::from_millis(300);

#[test]
fn a_load_that_panics_leaves_the_built_in_facts_with_its_reason() {
    assert!(active_platform_help_request().is_none(), "nothing may fix the snapshot early");
    let load = begin_platform_help_load();

    let (looked_up, answer) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let data = PlatformData::instance();
        looked_up.send((data.help_request().clone(), data.help_missing_reason())).unwrap();
    });
    assert!(answer.recv_timeout(SETTLE).is_err(), "the lookup answered before the load ended");

    let loader = std::thread::spawn(move || {
        let _load = load;
        panic!("the loader fails on purpose");
    });
    assert!(loader.join().is_err(), "the loader thread panicked");

    let (request, reason) =
        answer.recv_timeout(Duration::from_secs(30)).expect("the lookup was not woken");
    reader.join().unwrap();
    assert_eq!(request, PlatformHelpRequest::Bundled);
    assert!(reason.is_some_and(|reason| reason.contains("panic")), "{reason:?}");
    assert!(!platform_help_pending());
    assert!(!PlatformData::instance().all_types().is_empty(), "the built-in facts serve");

    // A configured source differs from the one serving, so a reload is refused with a
    // restart warning rather than taken for the library default staying in place.
    let configured = PlatformHelp::without_io(&PlatformHelpRequest::None).unwrap();
    let conflict = install_platform_help(configured).unwrap_err();
    assert_eq!(conflict.active, PlatformHelpRequest::Bundled);
}
