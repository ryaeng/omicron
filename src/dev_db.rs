//! Facilities for managing a local database for development

use anyhow::anyhow;
use anyhow::Context;
use std::ffi::OsStr;
use std::fmt;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;
use std::time::Instant;
use tempfile::tempdir;
use tempfile::TempDir;
use thiserror::Error;

/// Default for how long to wait for CockroachDB to report its listening URL
const COCKROACHDB_START_TIMEOUT_DEFAULT: Duration = Duration::from_secs(30);

/**
 * Builder for [`CockroachStarter`] that supports setting some command-line
 * arguments for the `cockroach start-single-node` command
 *
 * Without customizations, this will run `cockroach start-single-node --insecure
 * --listen-addr=127.0.0.1`.
 *
 * It's useful to support running this concurrently (as in the test suite).  To
 * support this, we allow CockroachDB to choose its listening ports.  To figure
 * out which ports it chose, we also use the --listening-url-file option to have
 * it write the URL to a file in a temporary directory.  The Drop
 * implementations for `CockroachStarter` and `CockroachInstance` will ensure
 * that this directory gets cleaned up as long as this program exits normally.
 */
#[derive(Debug)]
pub struct CockroachStarterBuilder {
    /// optional value for the --store-dir option
    store_dir: Option<PathBuf>,
    /// command-line arguments, mirrored here for reporting
    args: Vec<String>,
    /// describes the command line that we're going to execute
    cmd_builder: tokio::process::Command,
    /// how long to wait for CockroachDB to report itself listening
    start_timeout: Duration,
}

impl CockroachStarterBuilder {
    pub fn new() -> CockroachStarterBuilder {
        CockroachStarterBuilder::new_with_cmd("cockroach")
    }

    fn new_with_cmd(cmd: &str) -> CockroachStarterBuilder {
        let mut builder = CockroachStarterBuilder {
            store_dir: None,
            args: vec![String::from(cmd)],
            cmd_builder: tokio::process::Command::new(cmd),
            start_timeout: COCKROACHDB_START_TIMEOUT_DEFAULT,
        };

        /*
         * We use single-node insecure mode listening only on localhost.  We
         * consider this secure enough for development (including the test
         * suite), though it does allow anybody on the system to do anything
         * with this database (including fill up all disk space).  (It wouldn't
         * be unreasonable to secure this with certificates even though we're
         * on localhost.
         *
         * If we decide to let callers customize various listening addresses, we
         * should be careful about making it too easy to generate a more
         * insecure configuration.
         */
        builder
            .arg("start-single-node")
            .arg("--insecure")
            .arg("--listen-addr=127.0.0.1");
        builder
    }

    pub fn start_timeout(&mut self, duration: &Duration) -> &mut Self {
        self.start_timeout = *duration;
        self
    }

    /**
     * Sets the `--store-dir` command-line argument to `store_dir`
     *
     * This is where the database will store all of its on-disk data.  If this
     * isn't specified, CockroachDB will be configured to store data into a
     * temporary directory that will be cleaned up on Drop of
     * [`CockroachStarter`] or [`CockroachInstance`].
     */
    pub fn store_dir<P: AsRef<Path>>(mut self, store_dir: P) -> Self {
        self.store_dir.replace(store_dir.as_ref().to_owned());
        self
    }

    /**
     * Starts CockroachDB using the configured command-line arguments
     *
     * This will create a temporary directory for the listening URL file (see
     * above) and potentially the database store directory (if `store_dir()`
     * was never called).
     */
    pub fn build(mut self) -> Result<CockroachStarter, anyhow::Error> {
        /*
         * We always need a temporary directory, if for no other reason than to
         * put the listen-url file.  (It would be nice if the subprocess crate
         * allowed us to open a pipe stream to the child other than stdout or
         * stderr, although there may not be a portable means to identify it to
         * CockroachDB on the command line.)
         *
         * TODO Maybe it would be more ergonomic to use a well-known temporary
         * directory rather than a random one.  That way, we can warn the user
         * if they start up two of them, and we can also clean up after unclean
         * shutdowns.
         */
        let temp_dir =
            tempdir().with_context(|| "creating temporary directory")?;
        let store_dir = self
            .store_dir
            .as_ref()
            .map(|s| s.as_os_str().to_owned())
            .unwrap_or_else(|| {
                CockroachStarterBuilder::temp_path(&temp_dir, "data")
                    .into_os_string()
            });
        let listen_url_file =
            CockroachStarterBuilder::temp_path(&temp_dir, "listen-url");
        self.arg("--store")
            .arg(store_dir)
            .arg("--listening-url-file")
            .arg(listen_url_file.as_os_str().to_owned());
        Ok(CockroachStarter {
            temp_dir,
            listen_url_file,
            args: self.args,
            cmd_builder: self.cmd_builder,
            start_timeout: self.start_timeout,
        })
    }

    /**
     * Convenience wrapper for self.cmd_builder.arg() that records the arguments
     * so that we can print out the command line before we run it
     */
    fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Self {
        let arg = arg.as_ref();
        self.args.push(arg.to_string_lossy().to_string());
        self.cmd_builder.arg(arg);
        self
    }

    /**
     * Convenience for constructing a path name in a given temporary directory
     */
    fn temp_path<S: AsRef<str>>(tempdir: &TempDir, file: S) -> PathBuf {
        let mut pathbuf = tempdir.path().to_owned();
        pathbuf.push(file.as_ref());
        pathbuf
    }
}

/**
 * Manages execution of the `cockroach` command in order to start a CockroachDB
 * instance
 *
 * To use this, see [`CockroachStarterBuilder`].
 */
#[derive(Debug)]
pub struct CockroachStarter {
    /// temporary directory used for URL file and potentially data storage
    temp_dir: TempDir,
    /// path to listen URL file (inside temp_dir)
    listen_url_file: PathBuf,
    /// command-line arguments, mirrored here for reporting to the user
    args: Vec<String>,
    /// the command line that we're going to execute
    cmd_builder: tokio::process::Command,
    /// how long to wait for the listen URL to be written
    start_timeout: Duration,
}

impl CockroachStarter {
    /** Returns a human-readable summary of the command line to be executed */
    pub fn cmdline(&self) -> impl fmt::Display {
        self.args.join(" ")
    }

    /**
     * Returns the path to the temporary directory created for this execution
     */
    pub fn temp_dir(&self) -> &Path {
        self.temp_dir.path()
    }

    /**
     * Spawns a new process to run the configured command
     *
     * This function waits up to a fixed timeout for CockroachDB to report its
     * listening URL.  This function fails if the child process exits before
     * that happens or if the timeout expires.
     */
    pub async fn start(
        mut self,
    ) -> Result<CockroachInstance, CockroachStartError> {
        let mut child_process =
            self.cmd_builder.spawn().with_context(|| {
                format!(
                    "running \"{}\" (is the binary installed \
                        and on your PATH?)",
                    self.args[0]
                )
            })?;
        let pid = child_process.id().unwrap();
        let poll_interval = Duration::from_millis(10);
        let poll_start = Instant::now();
        let poll_max = &self.start_timeout;

        /*
         * Wait for CockroachDB to write out its URL information.  There's not a
         * great way for us to know when this has happened, unfortunately.  So
         * we just poll for it up to some maximum timeout.
         */
        loop {
            let duration = Instant::now().duration_since(poll_start);
            if duration.gt(poll_max) {
                /*
                 * Skip cleanup of the temporary directory so that Cockroach
                 * doesn't trip over its files being gone and so the user can
                 * debug what happened.
                 */
                self.temp_dir.into_path();
                return Err(CockroachStartError::TimedOut {
                    pid,
                    time_waited: duration,
                });
            }

            if process_exited(&mut child_process) {
                return Err(CockroachStartError::from(anyhow!(
                    "cockroach failed to start (see error output above)"
                )));
            }

            /*
             * TODO-robustness It would be nice if there were a version of
             * tokio::fs::read_to_string() that accepted a maximum byte count so
             * that this couldn't, say, use up all of memory.
             */
            match tokio::fs::read_to_string(&self.listen_url_file).await {
                /*
                 * Wait for the entire URL to be written out.  This includes a
                 * trailing newline.
                 */
                Ok(listen_url) if listen_url.contains('\n') => {
                    let listen_url = listen_url.trim_end();
                    let pg_config: tokio_postgres::config::Config =
                        listen_url.parse().with_context(|| {
                            format!("parsing listen URL \"{}\"", listen_url)
                        })?;
                    return Ok(CockroachInstance {
                        pid,
                        listen_url: listen_url.to_string(),
                        pg_config,
                        temp_dir_path: self.temp_dir.path().to_owned(),
                        temp_dir: Some(self.temp_dir),
                        child_process: Some(child_process),
                    });
                }

                _ => {
                    tokio::time::sleep(poll_interval).await;
                }
            }
        }
    }
}

#[derive(Debug, Error)]
pub enum CockroachStartError {
    #[error(
        "cockroach did not report listen URL after {time_waited:?} \
        (may still be running as pid {pid} and temporary directory \
        may still exist)"
    )]
    TimedOut { pid: u32, time_waited: Duration },

    #[error("cockroach failed to start")]
    Failed {
        #[from]
        source: anyhow::Error,
    },
}

/**
 * Manages a CockroachDB process running as a single-node cluster
 *
 * You are **required** to invoke [`CockroachInstance::wait_for_shutdown()`] or
 * [`CockroachInstance::cleanup()`] before this object is dropped.
 */
#[derive(Debug)]
pub struct CockroachInstance {
    /// child process id
    pid: u32,
    /// Raw listen URL provided by CockroachDB
    listen_url: String,
    /// PostgreSQL config to use to connect to CockroachDB as a SQL client
    pg_config: tokio_postgres::config::Config,
    /// handle to child process, if it hasn't been cleaned up already
    child_process: Option<tokio::process::Child>,
    /// handle to temporary directory, if it hasn't been cleaned up already
    temp_dir: Option<TempDir>,
    /// path to temporary directory
    temp_dir_path: PathBuf,
}

impl CockroachInstance {
    /** Returns the pid of the child process running CockroachDB */
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /**
     * Returns a printable form of the PostgreSQL config provided by
     * CockroachDB
     *
     * This is intended only for printing out.  To actually connect to
     * PostgreSQL, use [`CockroachInstance::pg_config()`].  (Ideally, that
     * object would impl a to_url() or the like, but it does not appear to.)
     */
    pub fn listen_url(&self) -> impl fmt::Display + '_ {
        &self.listen_url
    }

    /**
     * Returns PostgreSQL client configuration suitable for connecting to the
     * CockroachDB database
     */
    pub fn pg_config(&self) -> &tokio_postgres::config::Config {
        &self.pg_config
    }

    /**
     * Returns the path to the temporary directory created for this execution
     */
    pub fn temp_dir(&self) -> &Path {
        &self.temp_dir_path
    }

    /**
     * Waits for the child process to exit
     *
     * Note that CockroachDB will normally run forever unless the caller
     * arranges for it to be shutdown.
     */
    pub async fn wait_for_shutdown(&mut self) -> Result<(), anyhow::Error> {
        /* We do not care about the exit status of this process. */
        #[allow(unused_must_use)]
        {
            self.child_process
                .as_mut()
                .expect("cannot call wait_for_shutdown() after cleanup()")
                .wait()
                .await;
        }
        self.child_process = None;
        self.cleanup().await
    }

    /**
     * Cleans up the child process and temporary directory
     *
     * If the child process is still running, it will be killed with SIGKILL and
     * this function will wait for it to exit.  Then the temporary directory
     * will be cleaned up.
     */
    pub async fn cleanup(&mut self) -> Result<(), anyhow::Error> {
        /*
         * Kill the process and wait for it to exit so that we can remove the
         * temporary directory that we may have used to store its data.  We
         * don't care what the result of the process was.
         */
        if let Some(child_process) = self.child_process.as_mut() {
            child_process
                .start_kill()
                .context("sending SIGKILL to child process")?;
            child_process.wait().await.context("waiting for child process")?;
            self.child_process = None;
        }

        if let Some(temp_dir) = self.temp_dir.take() {
            temp_dir.close().context("cleaning up temporary directory")?;
        }

        Ok(())
    }
}

impl Drop for CockroachInstance {
    fn drop(&mut self) {
        /*
         * TODO-cleanup Ideally at this point we would run self.cleanup() to
         * kill the child process, wait for it to exit, and then clean up the
         * temporary directory.  However, we don't have an executor here with
         * which to run async/await code.  We could create one here, but it's
         * not clear how safe or sketchy that would be.  Instead, we expect that
         * the caller has done the cleanup already.  This won't always happen,
         * particularly for ungraceful failures.
         */
        if self.child_process.is_some() || self.temp_dir.is_some() {
            eprintln!(
                "WARN: dropped CockroachInstance without cleaning it up first \
                (there may still be a child process running and a \
                temporary directory leaked)"
            );

            /* Still, make a best effort. */
            #[allow(unused_must_use)]
            if let Some(child_process) = self.child_process.as_mut() {
                child_process.start_kill();
            }
            #[allow(unused_must_use)]
            if let Some(temp_dir) = self.temp_dir.take() {
                temp_dir.close();
            }
        }
    }
}

/**
 * Wrapper around tokio::process::Child::try_wait() so that we can unwrap() the
 * result in one place with this explanatory comment.
 *
 * The semantics of that function aren't as clear as we'd like.  The docs say:
 *
 * > If the child has exited, then `Ok(Some(status))` is returned. If the
 * > exit status is not available at this time then `Ok(None)` is returned.
 * > If an error occurs, then that error is returned.
 *
 * It seems we can infer that "the exit status is not available at this time"
 * means that the process has not exited.  After all, if it _had_ exited, we'd
 * fall into the first case.  It's not clear under what conditions this function
 * could ever fail.  It's not clear from the source that it's even possible.
 */
fn process_exited(child_process: &mut tokio::process::Child) -> bool {
    child_process.try_wait().unwrap().is_some()
}

/*
 * These are more integration tests than unit tests.
 */
/*
 * XXX TODO-coverage This was manually tested, but we should add automated test
 * cases for:
 * - just like the happy-path test, but with an explicitly-provided store
 *   directory
 * - cockroach is killed in the background.
 * - you can run this twice concurrently and get two different databases.
 *
 * These should verify that the child process is gone and the temporary
 * directory is cleaned up, except for the timeout case.
 *
 * It would be nice to have an integration test for "omicron_dev db-run" that
 * sends SIGINT to the session and verifies that it exits the way we expect, the
 * database shuts down, and the temporary directory is removed.
 */
#[cfg(test)]
mod test {
    use super::CockroachStartError;
    use super::CockroachStarterBuilder;
    use std::time::Duration;
    use std::time::Instant;
    use tokio::fs;

    /*
     * Test the happy path: start the database, run a query against the URL we
     * found, and then shut it down cleanly.
     */
    #[tokio::test]
    async fn test_setup_database() {
        /* Start a database using the default temporary store directory */
        let builder = CockroachStarterBuilder::new();
        let starter = builder.build().unwrap();
        eprintln!("will run: {}", starter.cmdline());
        let mut database =
            starter.start().await.expect("failed to start database");
        let pid = database.pid();
        eprintln!(
            "database pid {} listening at: {}",
            pid,
            database.listen_url()
        );

        /*
         * The database process should be running and the temporary directory
         * should exist with the Cockroach "data" directory in it.
         */
        assert!(process_running(pid));
        assert!(fs::metadata(database.temp_dir().join("data"))
            .await
            .expect("CockroachDB data directory is missing")
            .is_dir());

        /* Try to connect to it and run a query. */
        let conn_task = {
            let (client, connection) = database
                .pg_config()
                .connect(tokio_postgres::NoTls)
                .await
                .expect("failed to connect to newly-started database");

            let conn_task = tokio::spawn(async move { connection.await });

            let row = client
                .query_one("SELECT 12345", &[])
                .await
                .expect("basic query failed");
            assert_eq!(row.len(), 1);
            assert_eq!(row.get::<'_, _, i64>(0), 12345);

            conn_task
        };

        /*
         * With the client dropped, the connection should be shut down.  Wait
         * for the task we spawned.  Then clean up the server.
         */
        conn_task
            .await
            .expect("failed to wait for conn task")
            .expect("connection unexpectedly failed");
        database.cleanup().await.expect("failed to clean up database");

        /* Check that the database process is no longer running. */
        assert!(!process_running(pid));

        /* Check that the temporary directory we used has been cleaned up. */
        assert_eq!(
            libc::ENOENT,
            fs::metadata(database.temp_dir())
                .await
                .expect_err("temporary directory still exists")
                .raw_os_error()
                .unwrap()
        );
    }

    /*
     * Tests what happens if the "cockroach" command cannot be found.
     */
    #[tokio::test]
    async fn test_bad_cmd() {
        let builder = CockroachStarterBuilder::new_with_cmd("/nonexistent");
        test_database_start_failure(builder).await;
    }

    /*
     * Tests what happens if the "cockroach" command exits before writing the
     * listening-url file.  This looks the same to the caller (us), but
     * internally requires different code paths.
     */
    #[tokio::test]
    async fn test_cmd_fails() {
        let mut builder = CockroachStarterBuilder::new();
        builder.arg("not-a-valid-argument");
        test_database_start_failure(builder).await;
    }

    /*
     * Helper function for testing cases where the database fails to start.
     */
    async fn test_database_start_failure(builder: CockroachStarterBuilder) {
        let starter = builder.build().unwrap();
        let temp_dir = starter.temp_dir().to_owned();
        eprintln!("will run: {}", starter.cmdline());
        let error =
            starter.start().await.expect_err("unexpectedly started database");
        eprintln!("error: {:?}", error);
        assert_eq!(
            libc::ENOENT,
            fs::metadata(temp_dir)
                .await
                .expect_err("temporary directory still exists")
                .raw_os_error()
                .unwrap()
        );
    }

    /*
     * Tests that we clean up the temporary directory correctly when the starter
     * goes out of scope, even if we never started the instance.  This is
     * important to avoid leaking the directory if there's an error starting the
     * instance, for example.
     */
    #[tokio::test]
    async fn test_starter_tmpdir() {
        let builder = CockroachStarterBuilder::new();
        let starter = builder.build().unwrap();
        let directory = starter.temp_dir().to_owned();
        assert!(fs::metadata(&directory)
            .await
            .expect("temporary directory is missing")
            .is_dir());
        drop(starter);
        assert_eq!(
            libc::ENOENT,
            fs::metadata(&directory)
                .await
                .expect_err("temporary directory still exists")
                .raw_os_error()
                .unwrap()
        );
    }

    /*
     * Tests when CockroachDB hangs on startup by setting the start timeout
     * absurdly short.  This unfortunately doesn't cover all cases.  By choosing
     * a zero timeout, we're not letting the database get very far in its
     * startup.  But we at least ensure that the test suite does not hang or
     * timeout at some very long value.
     */
    #[tokio::test]
    async fn test_database_start_hang() {
        let mut builder = CockroachStarterBuilder::new();
        builder.start_timeout(&Duration::from_millis(0));
        let starter = builder.build().expect("failed to build starter");
        let directory = starter.temp_dir().to_owned();
        eprintln!("temporary directory: {}", directory.display());
        let error =
            starter.start().await.expect_err("unexpectedly started database");
        eprintln!("(expected) error starting database: {:?}", error);
        let pid = match error {
            CockroachStartError::TimedOut { pid, time_waited } => {
                /* We ought to fire a 0-second timeout within 5 seconds. */
                assert!(time_waited < Duration::from_secs(5));
                pid
            }
            other_error => panic!(
                "expected timeout error, but got some other error: {:?}",
                other_error
            ),
        };
        /* The child process should still be running. */
        assert!(process_running(pid));
        /* The temporary directory should still exist. */
        assert!(fs::metadata(&directory)
            .await
            .expect("temporary directory is missing")
            .is_dir());
        /* Kill the child process (to clean up after ourselves). */
        assert_eq!(0, unsafe { libc::kill(pid as libc::pid_t, libc::SIGKILL) });

        /*
         * Wait for the process to exit so that we can reliably clean up the
         * temporary directory.  We don't have a great way to avoid polling
         * here.
         * XXX This should be commonized with the backoff stuff and with the
         * other use of polling above.  We should have a test suite
         * configuration that makes clear from the name that it should not be
         * used in production.  It should be pretty aggressive so that the test
         * suite doesn't sleep for long periods at a time or take a lot longer
         * than it needs to.
         */
        let poll_interval = Duration::from_millis(25);
        let poll_start = Instant::now();
        let poll_max = Duration::from_secs(10);
        while process_running(pid) {
            let duration = Instant::now().duration_since(poll_start);
            if duration > poll_max {
                panic!(
                    "timed out waiting for pid {} to exit \
                    (leaving temporary directory {})",
                    pid,
                    directory.display()
                );
            }

            tokio::time::sleep(poll_interval).await;
        }

        assert!(!process_running(pid));
        /* XXX add a safety here */
        fs::remove_dir_all(&directory).await.unwrap_or_else(|e| {
            panic!(
                "failed to remove temporary directory {}: {:?}",
                directory.display(),
                e
            )
        });
    }

    fn process_running(pid: u32) -> bool {
        /*
         * It should be safe to invoke this syscall with these arguments.  This
         * only checks whether the process is running.
         */
        0 == (unsafe { libc::kill(pid as libc::pid_t, 0) })
    }
}
