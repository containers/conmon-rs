//! The threads which create the sockets in the network namespace.

use super::*;

#[tokio::test]
async fn namespace_threads_serve_close_connections() -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    let namespace = Namespace::with_linger(NETNS.into(), Duration::from_secs(60));
    for _ in 0..3 {
        let _connection = PortForward::connect(&namespace, port).await?;
        listener.accept().await?;
    }
    assert_eq!(namespace.threads.load(Ordering::Relaxed), 1);

    // The thread ends without connections, the next one starts another.
    let namespace = Namespace::with_linger(NETNS.into(), Duration::from_millis(10));
    let _connection = PortForward::connect(&namespace, port).await?;
    wait_for(|| namespace.ended.load(Ordering::Relaxed) != 0)
        .await
        .context("the thread lingers")?;
    let _connection = PortForward::connect(&namespace, port).await?;
    assert_eq!(namespace.threads.load(Ordering::Relaxed), 2);
    Ok(())
}

#[tokio::test]
async fn namespace_threads_end_with_their_session() -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    let namespace = Namespace::with_linger(NETNS.into(), Duration::from_secs(60));
    let _connection = PortForward::connect(&namespace, listener.local_addr()?.port()).await?;
    let ended = namespace.ended.clone();
    drop(namespace);
    wait_for(|| ended.load(Ordering::Relaxed) != 0)
        .await
        .context("the thread lingers")?;
    Ok(())
}

#[tokio::test]
async fn every_connection_gets_its_namespace_error() -> Result<()> {
    let namespace = Namespace::new("/nonexistent".into());
    let connects = (0..10).map(|_| PortForward::connect(&namespace, 80));
    for result in futures::future::join_all(connects).await {
        let err = result.expect_err("no namespace");
        assert!(
            err.chain().any(|e| e.downcast_ref::<io::Error>().is_some()),
            "{err:#}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn namespace_failures_are_not_kept() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("netns");
    let namespace = Namespace::new(path.clone());
    let err = PortForward::connect(&namespace, 80)
        .await
        .expect_err("no namespace");
    // The error keeps its chain.
    assert!(
        err.chain().any(|e| e.downcast_ref::<io::Error>().is_some()),
        "{err:#}"
    );
    assert!(
        format!("{err:#}").contains(&format!("open network namespace {}", path.display())),
        "{err:#}"
    );

    // Once the namespace exists, the next connection works.
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
    std::os::unix::fs::symlink(NETNS, &path)?;
    let _connection = PortForward::connect(&namespace, listener.local_addr()?.port()).await?;
    Ok(())
}
