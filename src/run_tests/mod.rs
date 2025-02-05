use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use dirs::home_dir;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use tracing::{debug, info, instrument};

use super::boot_fake_node::{compile_runtime, get_runtime_binary, run_runtime};
use super::build;
use super::inject_message;
use super::start_package;

pub mod cleanup;
use cleanup::{cleanup, cleanup_on_signal};
pub mod network_router;
pub mod types;
use types::*;
mod tester_types;
use tester_types as tt;

fn get_basename(file_path: &Path) -> Option<&str> {
    file_path
        .file_name()
        .and_then(|file_name| file_name.to_str())
}

fn expand_home_path_string(path: &str) -> Option<String> {
    if path.starts_with("~/") {
        if let Some(home_path) = home_dir() {
            return Some(path.replacen("~", &home_path.to_string_lossy(), 1));
        }
    }
    None
}

fn expand_home_path(path: &PathBuf) -> Option<PathBuf> {
    path.as_os_str()
        .to_str()
        .and_then(|s| expand_home_path_string(s))
        .and_then(|s| Some(Path::new(&s).to_path_buf()))
}

#[instrument(level = "trace", err, skip_all)]
fn make_node_names(nodes: Vec<Node>) -> anyhow::Result<Vec<String>> {
    nodes
        .iter()
        .map(|node| get_basename(&node.home)
            .and_then(|base| Some(base.to_string()))
            .and_then(|mut base| {
                if !base.ends_with(".os") {
                    base.push_str(".os");
                }
                Some(base)
            })
            .ok_or(anyhow::anyhow!(
                "run_tests:make_node_names: did not find basename for {:?}",
                node.home,
            ))
        )
        .collect()
}

impl Config {
    fn expand_home_paths(mut self: Config) -> Config {
        self.runtime = match self.runtime {
            Runtime::FetchVersion(version) => Runtime::FetchVersion(version),
            Runtime::RepoPath(runtime_path) => {
                Runtime::RepoPath(expand_home_path(&runtime_path).unwrap_or(runtime_path))
            },
        };
        for test in self.tests.iter_mut() {
            test.test_packages = test.test_packages
                .iter()
                .map(|tp| {
                    TestPackage {
                        path: expand_home_path(&tp.path).unwrap_or_else(|| tp.path.clone()),
                        grant_capabilities: tp.grant_capabilities.clone(),
                    }
                })
                .collect();
            for node in test.nodes.iter_mut() {
                node.home = expand_home_path(&node.home).unwrap_or_else(|| node.home.clone());
            }
        }
        self
    }
}

#[instrument(level = "trace", err, skip_all)]
async fn wait_until_booted(
    port: u16,
    max_waits: u16,
    mut recv_kill_in_wait: BroadcastRecvBool,
) -> anyhow::Result<()> {
    for _ in 0..max_waits {
        let request = inject_message::make_message(
            "vfs:distro:sys",
            Some(15),
            &serde_json::to_string(&serde_json::json!({
                "path": "/tester:sys/pkg",
                "action": "ReadDir",
            })).unwrap(),
            None,
            None,
            None,
        )?;

        match inject_message::send_request(
            &format!("http://localhost:{}", port),
            request,
        ).await {
            Ok(response) => match inject_message::parse_response(response).await {
                Ok(_) => return Ok(()),
                _ => (),
            }
            _ => (),
        }

        tokio::select! {
            _ = sleep(Duration::from_secs(1)) => {}
            _ = recv_kill_in_wait.recv() => {
                return Err(anyhow::anyhow!("received exit"));
            }
        };
    }
    Err(anyhow::anyhow!("kit run-tests: could not connect to Kinode"))
}

#[instrument(level = "trace", err, skip_all)]
async fn load_setups(setup_paths: &Vec<PathBuf>, port: u16) -> anyhow::Result<()> {
    info!("Loading setup packages...");

    for setup_path in setup_paths {
        start_package::execute(&setup_path, &format!("http://localhost:{}", port)).await?;
    }

    info!("Done loading setup packages.");
    Ok(())
}

#[instrument(level = "trace", err, skip_all)]
async fn load_tests(test_packages: &Vec<TestPackage>, port: u16) -> anyhow::Result<()> {
    info!("Loading tests...");

    for TestPackage { ref path, .. } in test_packages {
        let basename = get_basename(path).unwrap();
        let request = inject_message::make_message(
            "vfs:distro:sys",
            Some(15),
            &serde_json::to_string(&serde_json::json!({
                "path": format!("/tester:sys/tests/{basename}.wasm"),
                "action": "Write",
            })).unwrap(),
            None,
            None,
            path.join("pkg").join(format!("{basename}.wasm")).to_str(),
        )?;

        let response = inject_message::send_request(
            &format!("http://localhost:{}", port),
            request,
        ).await?;
        match inject_message::parse_response(response).await {
            Ok(_) => {},
            Err(e) => return Err(anyhow::anyhow!("Failed to load tests: {}", e)),
        }
    }

    let mut grant_caps = std::collections::HashMap::new();
    for TestPackage { ref path, ref grant_capabilities } in test_packages {
        grant_caps.insert(
            path.file_name().map(|f| f.to_str()).unwrap(),
            grant_capabilities,
        );
    }
    let grant_caps = serde_json::to_vec(&grant_caps)?;

    let request = inject_message::make_message(
        "vfs:distro:sys",
        Some(15),
        &serde_json::to_string(&serde_json::json!({
            "path": format!("/tester:sys/tests/grant_capabilities.json"),
            "action": "Write",
        })).unwrap(),
        None,
        Some(&grant_caps),
        None,
    )?;

    let response = inject_message::send_request(
        &format!("http://localhost:{}", port),
        request,
    ).await?;
    match inject_message::parse_response(response).await {
        Ok(_) => {},
        Err(e) => return Err(anyhow::anyhow!("Failed to load tests capabilities: {}", e)),
    }


    info!("Done loading tests.");
    Ok(())
}

#[instrument(level = "trace", err, skip_all)]
async fn run_tests(
    test_packages: &Vec<TestPackage>,
    mut ports: Vec<u16>,
    node_names: Vec<String>,
    test_timeout: u64,
) -> anyhow::Result<()> {
    let master_port = ports.remove(0);

    // Set up non-master nodes.
    for port in ports {
        let request = inject_message::make_message(
            "tester:tester:sys",
            Some(15),
            &serde_json::to_string(&serde_json::json!({
                "Run": {
                    "input_node_names": node_names,
                    "test_names": test_packages
                        .iter()
                        .map(|tp| tp.path.to_str().unwrap())
                        .collect::<Vec<&str>>(),
                    "test_timeout": test_timeout,
                }
            })).unwrap(),
            None,
            None,
            None,
        )?;
        let response = inject_message::send_request(
            &format!("http://localhost:{}", port),
            request,
        ).await?;

        if response.status() != 200 {
            return Err(anyhow::anyhow!("Failed with status code: {}", response.status()))
        }
    }

    // Set up master node & start tests.
    info!("Running tests...");
    let request = inject_message::make_message(
        "tester:tester:sys",
        Some(15),
        &serde_json::to_string(&serde_json::json!({
            "Run": {
                "input_node_names": node_names,
                "test_names": test_packages
                    .iter()
                    .map(|tp| tp.path.to_str().unwrap())
                    .collect::<Vec<&str>>(),
                "test_timeout": test_timeout,
            }
        })).unwrap(),
        None,
        None,
        None,
    )?;
    let response = inject_message::send_request(
        &format!("http://localhost:{}", master_port),
        request,
    ).await?;

    match inject_message::parse_response(response).await {
        Ok(inject_message::Response { ref body, .. }) => {
            match serde_json::from_str(body)? {
                tt::TesterResponse::Pass => info!("PASS"),
                tt::TesterResponse::Fail { test, file, line, column } => {
                    return Err(anyhow::anyhow!("FAIL: {} {}:{}:{}", test, file, line, column));
                },
                tt::TesterResponse::GetFullMessage(_) => {
                    return Err(anyhow::anyhow!("FAIL: Unexpected Response"));
                },
            }
        },
        Err(e) => {
            return Err(anyhow::anyhow!("FAIL: {}", e));
        },
    };

    Ok(())
}

#[instrument(level = "trace", err, skip_all)]
async fn handle_test(detached: bool, runtime_path: &Path, test: Test) -> anyhow::Result<()> {
    for setup_package_path in &test.setup_package_paths {
        build::execute(&setup_package_path, false, false, test.package_build_verbose, false).await?;
    }
    for TestPackage { ref path, .. } in &test.test_packages {
        build::execute(path, false, false, test.package_build_verbose, false).await?;
    }

    // Initialize variables for master node and nodes list
    let mut master_node_port = None;
    let mut task_handles = Vec::new();
    let node_handles = Arc::new(Mutex::new(Vec::new()));
    let node_cleanup_infos = Arc::new(Mutex::new(Vec::new()));

    // Cleanup, boot check, test loading, and running
    let (send_to_cleanup, recv_in_cleanup) = tokio::sync::mpsc::unbounded_channel();
    let (send_to_kill, _recv_kill) = tokio::sync::broadcast::channel(1);
    let recv_kill_in_cos = send_to_kill.subscribe();
    let recv_kill_in_router = send_to_kill.subscribe();
    let node_cleanup_infos_for_cleanup = Arc::clone(&node_cleanup_infos);
    let node_handles_for_cleanup = Arc::clone(&node_handles);
    let send_to_kill_for_cleanup = send_to_kill.clone();
    let handle = tokio::spawn(cleanup(
        recv_in_cleanup,
        send_to_kill_for_cleanup,
        node_cleanup_infos_for_cleanup,
        Some(node_handles_for_cleanup),
        detached,
        true,
    ));
    task_handles.push(handle);
    let send_to_cleanup_for_signal = send_to_cleanup.clone();
    let handle = tokio::spawn(cleanup_on_signal(send_to_cleanup_for_signal, recv_kill_in_cos));
    task_handles.push(handle);
    let _cleanup_context = CleanupContext::new(send_to_cleanup.clone());

    let network_router_port_for_router = test.network_router.port.clone();
    let network_router_defects_for_router = test.network_router.defects.clone();
    let handle = tokio::spawn(async move {
        let _ = network_router::execute(
            network_router_port_for_router,
            network_router_defects_for_router,
            recv_kill_in_router,
        ).await;
    });
    task_handles.push(handle);

    // TODO: can remove?
    std::thread::sleep(std::time::Duration::from_secs(1));

    // Process each node
    for node in &test.nodes {
        fs::create_dir_all(&node.home)?;
        let node_home = fs::canonicalize(&node.home)?;
        for dir in &["kernel", "kv", "sqlite", "vfs"] {
            let dir = node_home.join(dir);
            if dir.exists() {
                fs::remove_dir_all(&node_home.join(dir)).unwrap();
            }
        }

        let mut args = vec!["--fake-node-name", &node.fake_node_name];
        if let Some(ref rpc) = node.rpc {
            args.extend_from_slice(&["--rpc", rpc]);
        };
        if let Some(ref password) = node.password {
            args.extend_from_slice(&["--password", password]);
        };
        if node.is_testnet {
            args.extend_from_slice(&["--testnet"]);
        }

        let (runtime_process, master_fd) = run_runtime(
            &runtime_path,
            &node_home,
            node.port,
            test.network_router.port,
            &args[..],
            node.runtime_verbose,
            detached,
        )?;

        let mut node_cleanup_infos = node_cleanup_infos.lock().await;
        node_cleanup_infos.push(NodeCleanupInfo {
            master_fd,
            process_id: runtime_process.id() as i32,
            home: node_home.clone(),
        });

        if master_node_port.is_none() {
            master_node_port = Some(node.port.clone());
        }

        let mut node_handles = node_handles.lock().await;
        node_handles.push(runtime_process);
    }

    let mut ports = Vec::new();

    for node in &test.nodes {
        let node_home = fs::canonicalize(&node.home)?;
        info!("Setting up node {:?}...", node_home);
        let recv_kill_in_wait = send_to_kill.subscribe();
        wait_until_booted(node.port, 5, recv_kill_in_wait).await?;
        ports.push(node.port);
        info!("Done setting up node {:?} on port {}.", node_home, node.port);
    }

    for port in &ports {
        load_setups(&test.setup_package_paths, port.clone()).await?;
    }
    load_tests(&test.test_packages, master_node_port.unwrap().clone()).await?;

    let tests_result = run_tests(
        &test.test_packages,
        ports,
        make_node_names(test.nodes)?,
        test.timeout_secs,
    ).await;

    let _ = send_to_cleanup.send(true);
    for handle in task_handles {
        handle.await.unwrap();
    }

    tests_result?;

    Ok(())
}

#[instrument(level = "trace", err, skip_all)]
pub async fn execute(config_path: &str) -> anyhow::Result<()> {
    let detached = true; // TODO: to arg?

    let config_content = fs::read_to_string(config_path)?;
    let config = toml::from_str::<Config>(&config_content)?.expand_home_paths();

    debug!("{:?}", config);

    // TODO: factor out with boot_fake_node?
    let runtime_path = match config.runtime {
        Runtime::FetchVersion(ref version) => get_runtime_binary(version).await?,
        Runtime::RepoPath(runtime_path) => {
            if !runtime_path.exists() {
                return Err(anyhow::anyhow!(
                    "RepoPath {:?} does not exist.",
                    runtime_path,
                ));
            }
            if runtime_path.is_dir() {
                // Compile the runtime binary
                compile_runtime(
                    &runtime_path,
                    config.runtime_build_verbose,
                    config.runtime_build_release,
                )?;
                runtime_path.join("target")
                    .join(if config.runtime_build_release { "release" } else { "debug" })
                    .join("kinode")
            } else {
                return Err(anyhow::anyhow!(
                    "RepoPath {:?} must be a directory (the repo).",
                    runtime_path
                ));
            }
        },
    };

    for test in config.tests {
        handle_test(detached, &runtime_path, test).await?;
    }

    Ok(())
}
