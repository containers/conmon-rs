use super::*;
use libc::pid_t;

#[test]
fn init_signals() {
    let signals = Pause::init_signals();
    for signal in [
        Signal::SIGTERM,
        Signal::SIGINT,
        Signal::SIGQUIT,
        Signal::SIGCHLD,
    ] {
        assert!(signals.contains(signal));
    }
    assert!(!signals.contains(Signal::SIGHUP));
}

#[test]
fn mappings_buffer() -> Result<()> {
    let mappings = |m: &[&str]| m.iter().map(ToString::to_string).collect::<Vec<_>>();

    // All ranges end up in one buffer, normalized to one line each.
    assert_eq!(
        Pause::mappings_buffer(&mappings(&["0 1000 1", "1\t100000  65536"]), 4096)?,
        "0 1000 1\n1 100000 65536\n"
    );
    assert_eq!(Pause::mappings_buffer(&[], 4096)?, "");
    // Nothing gets opened or written without mappings.
    Pause::write_mappings(&[], Pid::from_raw(pid_t::MAX), false)?;

    for invalid in [&["0 1000"][..], &["0 1000 1 2"], &["0 1000 x"], &["0 -1 1"]] {
        assert!(Pause::mappings_buffer(&mappings(invalid), 4096).is_err());
    }

    // A later invalid mapping fails before anything gets written.
    assert!(Pause::mappings_buffer(&mappings(&["0 0 1", "1 x 1"]), 4096).is_err());

    // The kernel takes less than a page and a limited number of ranges.
    let line = "4294967295 4294967295 1";
    let many = vec![line.to_string(); Pause::MAX_MAPPINGS];
    let err = Pause::mappings_buffer(&many, 4096).unwrap_err();
    assert!(format!("{err:#}").contains("less than 4096"), "{err:#}");
    assert!(Pause::mappings_buffer(&many, 65536).is_ok());
    let too_many = vec!["0 0 1".to_string(); Pause::MAX_MAPPINGS + 1];
    let err = Pause::mappings_buffer(&too_many, 65536).unwrap_err();
    assert!(format!("{err:#}").contains("limit of 340"), "{err:#}");
    Ok(())
}
