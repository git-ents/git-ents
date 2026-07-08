//! Kernel-side piping between two child processes.

use std::process::{Command, Stdio};

/// Run `producer | consumer` with the pipe handed to the kernel: the
/// producer's stdout *is* the consumer's stdin, so the stream — a cache or
/// tree archive that can outgrow the machine's memory — never lands in this
/// process. Both exit statuses gate success: a producer that dies mid-stream
/// fails the pipe even when the consumer accepted the truncated input.
pub(crate) fn pipe(mut producer: Command, mut consumer: Command, what: &str) -> Result<(), String> {
    let producer_name = producer.get_program().to_string_lossy().into_owned();
    let consumer_name = consumer.get_program().to_string_lossy().into_owned();
    let mut producing = producer
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run {producer_name} for {what}: {e}"))?;
    let Some(stdout) = producing.stdout.take() else {
        let _killed = producing.kill();
        let _reaped = producing.wait();
        return Err(format!("{producer_name} gave no stdout for {what}"));
    };
    let consumed = consumer.stdin(Stdio::from(stdout)).status();
    // Release this process's copy of the pipe's read end before waiting on
    // the producer: with the consumer dead mid-stream, the producer only
    // sees EPIPE — and stops blocking on a full pipe — once no read end is
    // left open.
    drop(consumer);
    let produced = producing
        .wait()
        .map_err(|e| format!("{producer_name} did not complete for {what}: {e}"))?;
    let consumed =
        consumed.map_err(|e| format!("could not run {consumer_name} for {what}: {e}"))?;
    if !produced.success() {
        return Err(format!("{producer_name} failed for {what}: {produced}"));
    }
    if !consumed.success() {
        return Err(format!("{consumer_name} failed for {what}: {consumed}"));
    }
    Ok(())
}
