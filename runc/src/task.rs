/*
Copyright 2022 The Kuasar Authors.

Licensed under the Apache License, Version 2.0 (the "License");
you may not use this file except in compliance with the License.
You may obtain a copy of the License at

http://www.apache.org/licenses/LICENSE-2.0

Unless required by applicable law or agreed to in writing, software
distributed under the License is distributed on an "AS IS" BASIS,
WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
See the License for the specific language governing permissions and
limitations under the License.
*/

use std::{
    collections::HashMap,
    mem::forget,
    os::{
        fd::{AsRawFd, OwnedFd},
        unix::net::UnixListener,
    },
    process::exit,
    sync::Arc,
    time::Duration,
};

use anyhow::anyhow;
use containerd_sandbox::error;
use containerd_shim::{
    asynchronous::{
        monitor::{monitor_subscribe, monitor_unsubscribe},
        task::TaskService,
        util::{asyncify, read_spec},
    },
    container::Container,
    monitor::{Subject, Topic},
    processes::Process,
    protos::{shim::shim_ttrpc_async::create_task, ttrpc::asynchronous::Server},
};
use log::{debug, error};
use nix::{
    libc,
    unistd::{fork, pipe, ForkResult},
};
use signal_hook_tokio::Signals;
use tokio::{
    sync::{mpsc::channel, Mutex},
    time::sleep,
};

use crate::{
    common::{has_shared_pid_namespace, prepare_unix_socket},
    handle_signals, read_count,
    runc::{RuncContainer, RuncFactory},
};

use std::net::{SocketAddr, TcpListener, TcpStream};
use nix::unistd::dup;

pub fn fork_task_server(task_socket: &str, sandbox_parent_dir: &str) -> Result<(), anyhow::Error> {
    //prepare_unix_socket(task_socket)?;

        println!("Path: {:?}", task_socket);

    if task_socket.starts_with("tcp://") {
        //let addr = &task_socket[6..]; // Remove "tcp://" prefix
        //let task_listener = TcpListener::bind(addr)?;
        let (pipe_r, pipe_w) = pipe().map_err(|e| anyhow!("failed to create pipe {}", e))?;
        match unsafe { fork().map_err(|e| anyhow!("failed to fork task service {}", e))? } {
            ForkResult::Parent { child: _ } => {
                drop(pipe_r);
                forget(pipe_w);
                Ok(())
            }
            ForkResult::Child => {
                drop(pipe_w);
                prctl::set_child_subreaper(true).unwrap();
                // TODO set thread count
                let runtime = tokio::runtime::Runtime::new().unwrap();
                runtime.block_on(async move {
                    if let Err(e) = run_tcp_task_server(task_socket, pipe_r, sandbox_parent_dir).await {
                        error!("run task server failed {}", e);
                        exit(-1);
                    }
                });
                exit(0);
            }
        }
    } else {
        let task_listener = UnixListener::bind(task_socket)?;
        let (pipe_r, pipe_w) = pipe().map_err(|e| anyhow!("failed to create pipe {}", e))?;
        match unsafe { fork().map_err(|e| anyhow!("failed to fork task service {}", e))? } {
            ForkResult::Parent { child: _ } => {
                drop(pipe_r);
                drop(task_listener);
                forget(pipe_w);
                Ok(())
            }
            ForkResult::Child => {
                drop(pipe_w);
                prctl::set_child_subreaper(true).unwrap();
                // TODO set thread count
                let runtime = tokio::runtime::Runtime::new().unwrap();
                runtime.block_on(async move {
                    if let Err(e) = run_task_server(task_listener, pipe_r, sandbox_parent_dir).await {
                        error!("run task server failed {}", e);
                        exit(-1);
                    }
                });
                exit(0);
            }
        } 
    }

}

async fn run_tcp_task_server(
    task_socket: &str,
    exit_pipe: OwnedFd,
    sandbox_parent_dir: &str,
) -> error::Result<()> {
    let task = start_task_service(sandbox_parent_dir).await?;
    let containers = task.containers.clone();
    let task_service: HashMap<String, containerd_shim::protos::ttrpc::asynchronous::Service> =
        create_task(Arc::new(Box::new(task)));

    let mut server = Server::new().register_service(task_service);
    server = server
        .bind(task_socket)
        .map_err(|e| anyhow!("failed to add listener to server {:?}", e))?;
    server = server.set_domain_tcp();

    server
        .start()
        .await
        .map_err(|e| anyhow!("failed to start task server, {}", e))?;
    // wait parent exit
    asyncify(move || Ok(read_count(exit_pipe.as_raw_fd(), 1).unwrap_or_default()))
        .await
        .unwrap_or_default();

    // after parent exit, wait exit_signal so that if all containers is removed, we can exit.
    loop {
        if containers.lock().await.is_empty() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    server
        .shutdown()
        .await
        .map_err(|e| anyhow!("failed to shutdown task server {}", e))?;
    Ok(())
}

async fn run_task_server(
    listener: UnixListener,
    exit_pipe: OwnedFd,
    sandbox_parent_dir: &str,
) -> error::Result<()> {
    let task = start_task_service(sandbox_parent_dir).await?;
    let containers = task.containers.clone();
    let task_service: HashMap<String, containerd_shim::protos::ttrpc::asynchronous::Service> =
        create_task(Arc::new(Box::new(task)));
    let mut server = Server::new().register_service(task_service);
    server = server
        .add_listener(listener.as_raw_fd())
        .map_err(|e| anyhow!("failed to add listener to server {:?}", e))?;
    server = server.set_domain_unix();

    server
        .start()
        .await
        .map_err(|e| anyhow!("failed to start task server, {}", e))?;
    // wait parent exit
    asyncify(move || Ok(read_count(exit_pipe.as_raw_fd(), 1).unwrap_or_default()))
        .await
        .unwrap_or_default();

    // after parent exit, wait exit_signal so that if all containers is removed, we can exit.
    loop {
        if containers.lock().await.is_empty() {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }
    server
        .shutdown()
        .await
        .map_err(|e| anyhow!("failed to shutdown task server {}", e))?;
    Ok(())
}

async fn process_exits<F>(task: &TaskService<F, RuncContainer>) {
    let containers = task.containers.clone();
    let exit_signal = task.exit.clone();
    let mut s = monitor_subscribe(Topic::Pid)
        .await
        .expect("monitor subscribe failed");
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = exit_signal.wait() => {
                    debug!("sandbox exit, should break");
                    monitor_unsubscribe(s.id).await.unwrap_or_default();
                    return;
                },
                res = s.rx.recv() => {
                    if let Some(e) = res {
                        if let Subject::Pid(pid) = e.subject {
                            debug!("receive exit event: {}", &e);
                            let exit_code = e.exit_code;
                            for (_k, cont) in containers.lock().await.iter_mut() {
                                // pid belongs to container init process
                                if cont.init.pid == pid {
                                    // kill all children process if the container has a
                                    // private PID namespace
                                    if should_kill_all_on_exit(&cont.bundle).await {
                                        cont.kill(None, 9, true).await.unwrap_or_else(|e| {
                                            error!("failed to kill init's children: {}", e)
                                        });
                                    }
                                    cont.init.set_exited(exit_code).await;
                                    break;
                                }

                                // pid belongs to container common process
                                for (_exec_id, p) in cont.processes.iter_mut() {
                                    // set exit for exec process
                                    if p.pid == pid {
                                        p.set_exited(exit_code).await;
                                        break;
                                    }
                                }
                            }
                        }
                    } else {
                        monitor_unsubscribe(s.id).await.unwrap_or_default();
                        return;
                    }
                }
            }
        }
    });
}

async fn start_task_service(
    sandbox_parent_dir: &str,
) -> error::Result<TaskService<RuncFactory, RuncContainer>> {
    tokio::spawn(async move {
        let signals = Signals::new([libc::SIGTERM, libc::SIGINT, libc::SIGPIPE, libc::SIGCHLD])
            .expect("new signal failed");
        handle_signals(signals).await;
    });
    let (tx, mut rx) = channel(128);
    let factory = RuncFactory::new(sandbox_parent_dir);
    let task = TaskService {
        factory,
        containers: Arc::new(Mutex::new(HashMap::new())),
        namespace: "k8s.io".to_string(),
        exit: Arc::new(Default::default()),
        tx: tx.clone(),
    };

    process_exits(&task).await;

    tokio::spawn(async move {
        while let Some((_topic, e)) = rx.recv().await {
            debug!("received event {:?}", e);
        }
    });
    Ok(task)
}

async fn should_kill_all_on_exit(bundle_path: &str) -> bool {
    match read_spec(bundle_path).await {
        Ok(spec) => has_shared_pid_namespace(&spec),
        Err(e) => {
            error!(
                "failed to read spec when call should_kill_all_on_exit: {}",
                e
            );
            false
        }
    }
}
