//! Runs Nanocode services on Tokio while driving their native I/O reactor.

use nanocode_executor::{Builder, Runtime};
use nanocode_platform::{Host, HostFuture};
use std::{
    future::{Future, poll_fn},
    io,
    sync::OnceLock,
    time::{Duration, Instant, SystemTime},
};

static DRIVER: OnceLock<Runtime> = OnceLock::new();
static HOST: TokioHost = TokioHost;
static INSTALLED: OnceLock<Result<(), String>> = OnceLock::new();

/// Installs the process host once. It resolves Tokio's handle at each spawn, so independent
/// test runtimes never retain a handle to a previous, shut-down runtime.
pub fn init() -> io::Result<()> {
    INSTALLED
        .get_or_init(|| {
            let shell = nanocode_psh::shell().map_err(|error| error.to_string())?;
            nanocode_home::shell::choose(shell).map_err(|_| "shell already selected".to_owned())?;
            nanocode_psh_embedded::embed()
                .map_err(|_| "embedded shell already installed".to_owned())?;
            let driver = Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("tact-native-io")
                .build()
                .map_err(|error| error.to_string())?;
            DRIVER
                .set(driver)
                .map_err(|_| "native driver already installed".to_owned())?;
            nanocode_platform::install(&HOST).map_err(|error| error.to_string())
        })
        .clone()
        .map_err(io::Error::other)
}

/// Enters the native reactor only during each poll; guards never cross an await or migrate
/// between Tokio worker threads. Dropping this future still cancels its inner operation.
pub async fn scope<F: Future>(future: F) -> F::Output {
    init().expect("initialize Nanocode runtime");
    let mut future = std::pin::pin!(future);
    poll_fn(|context| {
        let _entered = DRIVER
            .get()
            .expect("call tact_runtime::init before scope")
            .enter();
        future.as_mut().poll(context)
    })
    .await
}

struct TokioHost;

impl Host for TokioHost {
    fn now(&self) -> Instant {
        tokio::time::Instant::now().into_std()
    }
    fn system_time(&self) -> SystemTime {
        SystemTime::now()
    }
    fn sleep(&self, duration: Duration) -> HostFuture {
        Box::pin(async move { tokio::time::sleep(duration).await })
    }
    fn fill_random(&self, bytes: &mut [u8]) -> io::Result<()> {
        getrandom::fill(bytes).map_err(|error| io::Error::other(error.to_string()))
    }
    fn spawn(&self, future: HostFuture) {
        tokio::spawn(scope(future));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };

    #[tokio::test(start_paused = true)]
    async fn platform_clock_and_deadlines_follow_tokio_time() {
        init().unwrap();
        let before = nanocode_platform::now();
        tokio::time::advance(Duration::from_secs(60)).await;
        assert_eq!(nanocode_platform::now() - before, Duration::from_secs(60));
        let deadline = nanocode_platform::now() + Duration::from_secs(30);
        nanocode_platform::sleep_until(deadline).await;
        assert_eq!(nanocode_platform::now(), deadline);
    }

    #[test]
    fn shell_subprocess_dispatch_preserves_output_and_status() {
        init().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["psh-shell", "-c", "printf shell-ready; exit 37"])
            .output()
            .unwrap();
        assert_eq!(output.stdout, b"shell-ready");
        assert_eq!(output.status.code(), Some(37));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn native_http_and_host_tasks_keep_tokio_context() {
        init().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).unwrap() > 0);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .unwrap();
        });
        let task = nanocode_platform::spawn_task(async move {
            // Tool futures need Tokio's context even when Nanocode spawns them.
            tokio::time::sleep(Duration::from_millis(1)).await;
            nanocode_platform::sleep(Duration::from_millis(1)).await;
            nanocode_http::Client::new()
                .get(format!("http://{address}/"))
                .send()
                .await
                .unwrap()
                .text()
                .await
                .unwrap()
        });
        assert_eq!(
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .unwrap()
                .unwrap(),
            "ok"
        );
        server.join().unwrap();
    }

    #[tokio::test]
    async fn abort_drops_host_task_without_blocking_tokio() {
        init().unwrap();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        struct OnDrop(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                let _ = self.0.take().unwrap().send(());
            }
        }
        let task = nanocode_platform::spawn_task(async move {
            let _guard = OnDrop(Some(dropped_tx));
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        });
        started_rx.await.unwrap();
        task.abort();
        tokio::time::timeout(Duration::from_secs(2), dropped_rx)
            .await
            .unwrap()
            .unwrap();
    }
}

// Shell subprocesses re-enter the host executable, including Rust's test executables. Dispatch
// before either libtest or the application's argument parser can consume the shell arguments.
#[ctor::ctor]
fn shell_process_entry() {
    if let Some(code) = nanocode_psh::serve_if_requested(|arguments| psh::run_with(arguments, &[]))
    {
        // ExitCode exposes equality but no numeric accessor; preserve every shell status.
        let status = (0..=u8::MAX)
            .find(|status| code == std::process::ExitCode::from(*status))
            .unwrap();
        std::process::exit(i32::from(status));
    }
}
