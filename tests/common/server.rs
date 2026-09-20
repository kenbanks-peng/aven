use std::ffi::OsStr;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::process::kill_child_and_join_threads;
use super::{TestEnv, command};

pub struct TestServer {
    child: Child,
    output: Arc<Mutex<String>>,
    stdout_thread: Option<JoinHandle<()>>,
    stderr_thread: Option<JoinHandle<()>>,
    pub url: String,
}

impl TestServer {
    pub fn start(env: &TestEnv) -> Self {
        Self::start_with_data(env, "server.sqlite")
    }

    pub fn start_configured(env: &TestEnv, data: &str) -> Self {
        Self::start_configured_with_env(env, data, std::iter::empty::<(&str, &str)>())
    }

    pub fn start_configured_with_env<I, K, V>(env: &TestEnv, data: &str, envs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        Self::start_with_data_and_config(env, data, Some(env.config_dir().join("aven")), envs)
    }

    pub fn start_with_data(env: &TestEnv, data: &str) -> Self {
        Self::start_with_data_and_config(env, data, None, std::iter::empty::<(&str, &str)>())
    }

    fn start_with_data_and_config<I, K, V>(
        env: &TestEnv,
        data: &str,
        config_dir: Option<PathBuf>,
        envs: I,
    ) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        let output = Arc::new(Mutex::new(String::new()));
        let (url_tx, url_rx) = mpsc::channel();
        let mut command = command();
        env.configure_command(&mut command);
        command.args([
            "server",
            "--bind",
            "127.0.0.1:0",
            "--data",
            env.path(data).to_str().expect("utf8 temp path"),
        ]);
        if let Some(config_dir) = config_dir {
            command
                .env("AVEN_CONFIG_DIR", config_dir)
                .env_remove("AVEN_DB")
                .env_remove("AVEN_SYNC_SERVER");
        }
        for (key, value) in envs {
            command.env(key, value);
        }
        let mut child = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("spawn aven server");

        let stdout = child.stdout.take().expect("server stdout");
        let stdout_output = Arc::clone(&output);
        let stdout_thread = thread::spawn(move || {
            let reader = BufReader::new(stdout);
            for line in reader.lines().map_while(Result::ok) {
                {
                    let mut output = stdout_output.lock().expect("server output lock");
                    output.push_str(&line);
                    output.push('\n');
                }
                if let Some(rest) = line.strip_prefix("listening url=") {
                    let url = rest.split_whitespace().next().expect("listening url value");
                    let _ = url_tx.send(url.to_string());
                }
            }
        });

        let stderr = child.stderr.take().expect("server stderr");
        let stderr_output = Arc::clone(&output);
        let stderr_thread = thread::spawn(move || {
            let reader = BufReader::new(stderr);
            for line in reader.lines().map_while(Result::ok) {
                let mut output = stderr_output.lock().expect("server output lock");
                output.push_str(&line);
                output.push('\n');
            }
        });

        let deadline = Instant::now() + Duration::from_secs(10);
        let url = loop {
            if let Ok(url) = url_rx.try_recv() {
                break url;
            }
            if let Some(status) = child.try_wait().expect("check server status") {
                panic!(
                    "server exited during startup: {status}\n{}",
                    output.lock().expect("server output lock")
                );
            }
            assert!(
                Instant::now() < deadline,
                "server did not print listening url\n{}",
                output.lock().expect("server output lock")
            );
            thread::sleep(Duration::from_millis(50));
        };

        Self {
            child,
            output,
            stdout_thread: Some(stdout_thread),
            stderr_thread: Some(stderr_thread),
            url,
        }
    }

    pub fn output(&self) -> String {
        self.output.lock().expect("server output lock").clone()
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        kill_child_and_join_threads(
            &mut self.child,
            &mut self.stdout_thread,
            &mut self.stderr_thread,
        );
    }
}
