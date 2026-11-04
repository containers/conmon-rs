//! Resize events of the client.

use super::*;

#[tokio::test]
async fn resize_without_terminal_is_ignored() -> Result<()> {
    let io = SharedContainerIO::new(ContainerIO::new(false, ContainerLog::new())?);
    let resize = |width, height| io.try_resize(width, height);
    SessionInput::try_resize(br#"{"Width":80,"Height":24}"#, resize)?;

    // Garbage is still an error.
    assert!(SessionInput::try_resize(b"x", resize).is_err());
    Ok(())
}

#[test]
fn resize_events_match_fields_like_go() -> Result<()> {
    let size = |width, height| ResizeEvent { width, height };
    for (json, event) in [
        (r#"{"Width":80,"Height":24}"#, size(80, 24)),
        // The Kubernetes client for JavaScript.
        (r#"{"height":24,"width":80}"#, size(80, 24)),
        (r#"{"WIDTH":80,"hEiGhT":24,"Other":1}"#, size(80, 24)),
        (r#"{"Width":80}"#, size(80, 0)),
        (r#"{"Width":80,"Height":null}"#, size(80, 0)),
        ("{}", size(0, 0)),
        // The last of the names which match wins, a null changes
        // nothing.
        (r#"{"width":100,"Width":80,"height":24}"#, size(80, 24)),
        (r#"{"Width":80,"width":100,"height":24}"#, size(100, 24)),
        (r#"{"Width":80,"width":null,"height":24}"#, size(80, 24)),
        // Without a size, which gets ignored.
        ("null", size(0, 0)),
    ] {
        assert_eq!(ResizeEvent::parse(json.as_bytes())?, event, "{json}");
    }
    for json in [
        r#"{"Width":-1}"#,
        r#"{"Height":65536}"#,
        r#"{"Width":"80"}"#,
        "[]",
    ] {
        assert!(ResizeEvent::parse(json.as_bytes()).is_err(), "{json}");
    }
    Ok(())
}

#[tokio::test]
async fn resize_events_without_a_size_are_ignored() -> Result<()> {
    // A terminal which the runtime did not connect keeps the size which
    // reaches it for the connection.
    let (io, _child, _token) = attach_child(true)?;
    let resize = |width, height| io.try_resize(width, height);
    let pending = || io.terminal().and_then(|t| t.resizer().pending());
    for json in [
        r#"{"width":0,"height":0}"#,
        r#"{"width":100}"#,
        "{}",
        "null",
    ] {
        SessionInput::try_resize(json.as_bytes(), resize)?;
    }
    assert_eq!(pending(), None);
    SessionInput::try_resize(br#"{"width":80,"height":24}"#, resize)?;
    assert_eq!(pending(), Some((80, 24)));
    Ok(())
}
