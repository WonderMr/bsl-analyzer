//! A server whose platform help loader thread cannot start selects the help in place, from
//! the configured source, before it goes on: nothing is left announced and waiting, and the
//! library default does not take the configured source's place. Runs in its own test binary
//! because the selection is process-wide.

use bsl_platform::{
    active_platform_help_request, platform_help_pending, PlatformData, PlatformHelpRequest,
};

const FIXTURE: &str = include_str!("../../bsl-platform/tests/fixtures/help/corpus.json");

#[test]
fn a_loader_that_cannot_start_selects_the_configured_help_in_place() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("corpus.json"), FIXTURE).unwrap();
    std::fs::write(
        dir.path().join("bsl-analyzer.toml"),
        "[platform_help]\nsource = \"external\"\npath = \"corpus.json\"\n",
    )
    .unwrap();

    let mut asked = false;
    bsl_analyzer::help_bootstrap::spawn_bootstrap_on(Some(dir.path().to_path_buf()), |load| {
        asked = true;
        drop(load);
        Err(std::io::Error::other("no thread for this test"))
    });

    assert!(asked, "the thread start was asked for");
    assert!(!platform_help_pending(), "nothing is left announced");
    match active_platform_help_request() {
        Some(PlatformHelpRequest::ExternalPath(path)) => {
            assert!(path.ends_with("corpus.json"), "{}", path.display());
        }
        other => panic!("the configured source does not serve: {other:?}"),
    }
    let data = PlatformData::instance();
    assert!(data.help_origin().is_some(), "{:?}", data.help_missing_reason());
    assert!(data.get_method("Массив", "Добавить").is_some());
}
