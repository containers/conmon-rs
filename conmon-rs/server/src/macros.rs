/// Lock a mutex and return a possible poison error converted to an `anyhow::Error`.
/// ```ignore
/// let mutex = Mutex::new(vec![]);
/// // needs to be inside of a function that returns an anyhow::Result
/// lock!(mutex).push(42);
/// assert_eq!(lock!(mutex).pop(), Some(42));
/// ```
macro_rules! lock {
    ($mutex:expr_2021) => {
        $mutex.lock().map_err(|e| anyhow::anyhow!("{e}"))?
    };
}
