use cfy_process::{OutputMode, OutputStream, ProcessSpec, Supervisor};
use std::{
    env,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::Duration,
};

const HELPER_TEST: &str = "process_helper_entry";

#[test]
fn process_helper_entry() {
    let Ok(mode) = env::var("CFY_PROCESS_HELPER") else {
        return;
    };

    match mode.as_str() {
        "output" => {
            print!("captured stdout");
            eprint!("captured stderr");
            std::process::exit(23);
        }
        "sleep" => loop {
            thread::sleep(Duration::from_secs(1));
        },
        "tree-parent" => {
            let heartbeat = required_path("CFY_HEARTBEAT_PATH");
            let mut descendant = Command::new(env::current_exe().expect("test executable path"));
            descendant
                .arg("--exact")
                .arg(HELPER_TEST)
                .arg("--nocapture")
                .env("CFY_PROCESS_HELPER", "tree-child")
                .env("CFY_HEARTBEAT_PATH", heartbeat)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let mut descendant = descendant.spawn().expect("spawn heartbeat descendant");
            loop {
                assert!(
                    descendant
                        .try_wait()
                        .expect("check heartbeat descendant")
                        .is_none(),
                    "heartbeat descendant exited unexpectedly"
                );
                thread::sleep(Duration::from_millis(100));
            }
        }
        "tree-child" => {
            let heartbeat = required_path("CFY_HEARTBEAT_PATH");
            loop {
                let mut file = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&heartbeat)
                    .expect("open heartbeat file");
                file.write_all(b"x").expect("write heartbeat");
                file.flush().expect("flush heartbeat");
                thread::sleep(Duration::from_millis(40));
            }
        }
        other => panic!("unknown helper mode: {other}"),
    }
}

#[tokio::test]
async fn preserves_exit_code_and_captures_both_output_streams() {
    let supervisor = Supervisor::default();
    let output = supervisor
        .spawn(helper_spec("output", OutputMode::Capture))
        .unwrap()
        .wait()
        .await
        .unwrap();

    assert_eq!(output.exit_code(), Some(23));
    assert!(output.stdout.ends_with(b"captured stdout"));
    assert!(output.stderr.ends_with(b"captured stderr"));
    assert!(!output.cancelled);
}

#[tokio::test]
async fn streams_and_captures_output_without_mixing_channels() {
    let supervisor = Supervisor::default();
    let mut process = supervisor
        .spawn(helper_spec("output", OutputMode::CaptureAndStream))
        .unwrap();
    let mut streamed_stdout = Vec::new();
    let mut streamed_stderr = Vec::new();

    while let Some(chunk) = process.next_output().await {
        match chunk.stream {
            OutputStream::Stdout => streamed_stdout.extend(chunk.bytes),
            OutputStream::Stderr => streamed_stderr.extend(chunk.bytes),
        }
    }
    let output = process.wait().await.unwrap();

    assert_eq!(streamed_stdout, output.stdout);
    assert_eq!(streamed_stderr, output.stderr);
}

#[tokio::test]
async fn shutdown_cancels_multiple_children_and_drains_registry() {
    let supervisor = Supervisor::new(Duration::from_millis(200));
    let first = supervisor
        .spawn(helper_spec("sleep", OutputMode::Capture))
        .unwrap();
    let second = supervisor
        .spawn(helper_spec("sleep", OutputMode::Capture))
        .unwrap();
    assert_eq!(supervisor.active_count(), 2);

    supervisor.shutdown().await.unwrap();
    let first = first.wait().await.unwrap();
    let second = second.wait().await.unwrap();

    assert!(first.cancelled);
    assert!(second.cancelled);
    assert_eq!(supervisor.active_count(), 0);
}

#[tokio::test]
async fn cancellation_stops_descendants_in_the_same_process_tree() {
    let directory = unique_temp_directory();
    fs::create_dir_all(&directory).unwrap();
    let heartbeat = directory.join("heartbeat");
    let supervisor = Supervisor::new(Duration::from_millis(200));
    let process = supervisor
        .spawn(
            helper_spec("tree-parent", OutputMode::Capture)
                .env("CFY_HEARTBEAT_PATH", heartbeat.display().to_string()),
        )
        .unwrap();

    wait_for_heartbeat(&heartbeat).await;
    process.cancel();
    let output = process.wait().await.unwrap();
    assert!(output.cancelled);

    tokio::time::sleep(Duration::from_millis(120)).await;
    let size_after_cancel = fs::metadata(&heartbeat).unwrap().len();
    tokio::time::sleep(Duration::from_millis(250)).await;
    let final_size = fs::metadata(&heartbeat).unwrap().len();

    assert_eq!(final_size, size_after_cancel, "descendant kept running");
    fs::remove_dir_all(directory).unwrap();
}

fn helper_spec(mode: &str, output: OutputMode) -> ProcessSpec {
    ProcessSpec::new(env::current_exe().unwrap().display().to_string())
        .args(["--exact", HELPER_TEST, "--nocapture"])
        .env("CFY_PROCESS_HELPER", mode)
        .output(output)
}

fn required_path(name: &str) -> PathBuf {
    env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("missing {name}"))
}

fn unique_temp_directory() -> PathBuf {
    env::temp_dir().join(format!(
        "cfy-process-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

async fn wait_for_heartbeat(path: &Path) {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if fs::metadata(path).is_ok_and(|metadata| metadata.len() >= 2) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("descendant did not write a heartbeat");
}
