// Detect all kernelspecs available on the system,
// watch for changes to the kernelspecs directory,

// Since runtimelib uses tokio, we'll only use `runtimelib::dirs` for paths and reimplement
// the rest using `project::Fs`.

use anyhow::{Context as _, Result};
use futures::channel::mpsc;
use futures::{io::BufReader, AsyncBufReadExt, StreamExt};
use gpui::EntityId;
use project::Fs;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::process::Stdio;
use std::{path::PathBuf, sync::Arc};
use util::ResultExt as _;

use smol::net::TcpListener;

use smol::process::Command;

use runtimelib::{dirs, ConnectionInfo, JupyterKernelspec};

use crate::tokio_kernel::{connect_tokio_kernel_interface, ExecutionRequest};

#[derive(Debug, Clone)]
pub struct Runtime {
    pub name: String,
    pub path: PathBuf,
    pub spec: JupyterKernelspec,
}

impl Runtime {
    #[must_use]
    pub fn command(&self, connection_path: &PathBuf) -> Result<Command> {
        let argv = &self.spec.argv;

        if argv.is_empty() {
            return Err(anyhow::anyhow!("Empty argv in kernelspec {}", self.name));
        }

        if argv.len() < 2 {
            return Err(anyhow::anyhow!("Invalid argv in kernelspec {}", self.name));
        }

        if !argv.contains(&"{connection_file}".to_string()) {
            return Err(anyhow::anyhow!(
                "Missing 'connection_file' in argv in kernelspec {}",
                self.name
            ));
        }

        let mut cmd = Command::new(&argv[0]);

        for arg in &argv[1..] {
            if arg == "{connection_file}" {
                cmd.arg(connection_path);
            } else {
                cmd.arg(arg);
            }
        }

        if let Some(env) = &self.spec.env {
            cmd.envs(env);
        }

        Ok(cmd)
    }
}

// Find a set of open ports. This creates a listener with port set to 0. The listener will be closed at the end when it goes out of scope.
// There's a race condition between closing the ports and usage by a kernel, but it's inherent to the Jupyter protocol.
async fn peek_ports(ip: IpAddr, num: usize) -> anyhow::Result<Vec<u16>> {
    let mut addr_zeroport: SocketAddr = SocketAddr::new(ip, 0);
    addr_zeroport.set_port(0);
    let mut ports: Vec<u16> = Vec::new();
    for _ in 0..num {
        let listener = TcpListener::bind(addr_zeroport).await?;
        let addr = listener.local_addr()?;
        ports.push(addr.port());
    }
    Ok(ports)
}

pub async fn from_peeking_ports(ip: IpAddr, kernel_name: &str) -> Result<ConnectionInfo> {
    let transport = "tcp".to_string();
    let ports = peek_ports(ip, 5).await?;

    Ok(ConnectionInfo {
        transport,
        ip: ip.to_string(),
        stdin_port: ports[0],
        control_port: ports[1],
        hb_port: ports[2],
        shell_port: ports[3],
        iopub_port: ports[4],
        signature_scheme: "hmac-sha256".to_string(),
        key: uuid::Uuid::new_v4().to_string(),
        kernel_name: Some(kernel_name.to_string()),
    })
}

pub struct RunningKernel {
    runtime: Runtime,
    process: smol::process::Child,
    pub execution_request_tx: mpsc::UnboundedSender<ExecutionRequest>,
    _runtime_handle: std::thread::JoinHandle<()>,
}

impl RunningKernel {
    pub async fn new(
        runtime: Runtime,
        entity_id: &EntityId,
        fs: Arc<dyn Fs>,
    ) -> anyhow::Result<Self> {
        let connection_info = from_peeking_ports(
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            &format!("zed-{}", runtime.name),
        )
        .await?;

        let connection_path = dirs::runtime_dir().join(format!("kernel-zed-{}.json", entity_id));
        let content = serde_json::to_string(&connection_info)?;
        // write out file to disk for kernel
        fs.atomic_write(connection_path.clone(), content).await?;

        let mut cmd = runtime.command(&connection_path)?;
        let mut process = cmd
            .stdout(Stdio::piped())
            // .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("failed to start the kernel process")?;

        let stdout = process
            .stdout
            .take()
            .context("failed to get stdout for process")?;

        // let stderr = process
        //     .stderr
        //     .take()
        //     .context("failed to get stderr for process")?;

        // Wait for at least one line from stdout
        let stdout = BufReader::new(stdout);
        let mut lines = stdout.lines();
        let line = lines.next().await;
        println!("READ: {:?}", line);

        let (execution_request_tx, _runtime_handle) = connect_kernel(connection_info.clone())?;
        // Drop the connection info so the kernel will bind
        drop(connection_info);

        Ok(Self {
            runtime,
            process,
            execution_request_tx,
            _runtime_handle,
        })
    }
}

pub fn connect_kernel(
    connection_info: ConnectionInfo,
) -> Result<(
    mpsc::UnboundedSender<ExecutionRequest>,
    std::thread::JoinHandle<()>,
)> {
    let (execution_request_tx, execution_request_rx) = mpsc::unbounded::<ExecutionRequest>();

    let _runtime_handle = std::thread::spawn(|| {
        let tokio_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build();

        let tokio_runtime = match tokio_runtime {
            Ok(tokio_runtime) => tokio_runtime,
            Err(e) => {
                log::error!("Failed to create tokio runtime for jupyter kernel: {e:?}");
                return;
            }
        };

        // TODO: Will need a signal handler to shutdown the runtime
        tokio_runtime
            .block_on(async move {
                connect_tokio_kernel_interface(&connection_info, execution_request_rx).await
            })
            .log_err();
    });

    Ok((execution_request_tx.clone(), _runtime_handle))
}

pub async fn read_kernelspec_at(
    // Path should be a directory to a jupyter kernelspec, as in
    // /usr/local/share/jupyter/kernels/python3
    kernel_dir: PathBuf,
    fs: Arc<dyn Fs>,
) -> anyhow::Result<Runtime> {
    let path = kernel_dir;
    let kernel_name = if let Some(kernel_name) = path.file_name() {
        kernel_name.to_string_lossy().to_string()
    } else {
        return Err(anyhow::anyhow!("Invalid kernelspec directory: {:?}", path));
    };

    if !fs.is_dir(path.as_path()).await {
        return Err(anyhow::anyhow!("Not a directory: {:?}", path));
    }

    let expected_kernel_json = path.join("kernel.json");
    let spec = fs.load(expected_kernel_json.as_path()).await?;
    let spec = serde_json::from_str::<JupyterKernelspec>(&spec)?;

    Ok(Runtime {
        name: kernel_name,
        path,
        spec,
    })
}

/// Read a directory of kernelspec directories
pub async fn read_kernels_dir(path: PathBuf, fs: Arc<dyn Fs>) -> anyhow::Result<Vec<Runtime>> {
    let mut kernelspec_dirs = fs.read_dir(&path).await?;

    let mut valid_kernelspecs = Vec::new();
    while let Some(path) = kernelspec_dirs.next().await {
        match path {
            Ok(path) => {
                if fs.is_dir(path.as_path()).await {
                    let fs = fs.clone();
                    if let Ok(kernelspec) = read_kernelspec_at(path, fs).await {
                        valid_kernelspecs.push(kernelspec);
                    }
                }
            }
            Err(err) => {
                log::warn!("Error reading kernelspec directory: {:?}", err);
            }
        }
    }

    Ok(valid_kernelspecs)
}

pub async fn get_runtimes(fs: Arc<dyn Fs>) -> anyhow::Result<Vec<Runtime>> {
    let data_dirs = dirs::data_dirs();
    let kernel_dirs = data_dirs
        .iter()
        .map(|dir| dir.join("kernels"))
        .map(|path| read_kernels_dir(path, fs.clone()))
        .collect::<Vec<_>>();

    let kernel_dirs = futures::future::join_all(kernel_dirs).await;
    let kernel_dirs = kernel_dirs
        .into_iter()
        .filter_map(Result::ok)
        .flatten()
        .collect::<Vec<_>>();

    Ok(kernel_dirs)
}

#[cfg(test)]
mod test {
    use super::*;
    use std::path::PathBuf;

    use gpui::TestAppContext;
    use project::FakeFs;
    use serde_json::json;

    #[gpui::test]
    async fn test_get_kernelspecs(cx: &mut TestAppContext) {
        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(
            "/jupyter",
            json!({
                ".zed": {
                    "settings.json": r#"{ "tab_size": 8 }"#,
                    "tasks.json": r#"[{
                        "label": "cargo check",
                        "command": "cargo",
                        "args": ["check", "--all"]
                    },]"#,
                },
                "kernels": {
                    "python": {
                        "kernel.json": r#"{
                            "display_name": "Python 3",
                            "language": "python",
                            "argv": ["python3", "-m", "ipykernel_launcher", "-f", "{connection_file}"],
                            "env": {}
                        }"#
                    },
                    "deno": {
                        "kernel.json": r#"{
                            "display_name": "Deno",
                            "language": "typescript",
                            "argv": ["deno", "run", "--unstable", "--allow-net", "--allow-read", "https://deno.land/std/http/file_server.ts", "{connection_file}"],
                            "env": {}
                        }"#
                    }
                },
            }),
        )
        .await;

        let mut kernels = read_kernels_dir(PathBuf::from("/jupyter/kernels"), fs)
            .await
            .unwrap();

        kernels.sort_by(|a, b| a.name.cmp(&b.name));

        assert_eq!(
            kernels.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
            vec!["deno", "python"]
        );
    }
}
