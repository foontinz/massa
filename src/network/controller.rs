use std::error::Error;
type BoxResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

use super::peer_database::*;
use chrono::Utc;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use log::warn;
use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{sleep, Duration};

use super::config::NetworkConfig;

pub struct NetworkController {
    stop_tx: oneshot::Sender<()>,
    upstream_command_tx: mpsc::Sender<UpstreamCommand>,
    event_rx: mpsc::Receiver<NetworkControllerEvent>,
    controller_fn_handle: JoinHandle<()>,
}

#[derive(Debug)]
pub enum NetworkControllerEvent {
    CandidateConnection { ip: IpAddr, socket: TcpStream },
}

#[derive(Clone, Copy, Debug)]
pub enum PeerClosureReason {
    Normal,
    ConnectionFailed,
    Banned,
}

#[derive(Debug)]
pub enum UpstreamCommand {
    MergePeerList {
        ips: HashSet<IpAddr>,
    },
    GetAdvertisablePeerList {
        response_tx: oneshot::Sender<Vec<IpAddr>>,
    },
    PeerClosed {
        ip: IpAddr,
        reason: PeerClosureReason,
    },
    PeerAlive {
        ip: IpAddr,
    },
}

impl NetworkController {
    pub async fn new(cfg: NetworkConfig) -> BoxResult<Self> {
        let peer_db = PeerDatabase::load(
            cfg.known_peers_file.clone(),
            cfg.peer_file_dump_interval_seconds,
        )
        .await?;

        // launch controller
        let (stop_tx, stop_rx) = oneshot::channel::<()>();
        let (upstream_command_tx, upstream_command_rx) = mpsc::channel::<UpstreamCommand>(1024);
        let (event_tx, event_rx) = mpsc::channel::<NetworkControllerEvent>(1024);
        let controller_fn_handle = tokio::spawn(async move {
            controller_fn(cfg, peer_db, stop_rx, upstream_command_rx, event_tx).await;
        });

        Ok(NetworkController {
            stop_tx,
            upstream_command_tx,
            event_rx,
            controller_fn_handle,
        })
    }

    pub async fn stop(self) -> BoxResult<()> {
        let _ = self.stop_tx.send(());
        self.controller_fn_handle.await?;
        Ok(())
    }

    pub async fn wait_event(&mut self) -> BoxResult<NetworkControllerEvent> {
        self.event_rx
            .recv()
            .await
            .ok_or("error reading events".into())
    }

    pub fn get_upstream_interface(&self) -> NetworkControllerUpstreamInterface {
        NetworkControllerUpstreamInterface {
            upstream_command_tx: self.upstream_command_tx.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct NetworkControllerUpstreamInterface {
    upstream_command_tx: mpsc::Sender<UpstreamCommand>,
}

impl NetworkControllerUpstreamInterface {
    pub async fn merge_peer_list(
        &mut self,
        ips: HashSet<IpAddr>,
    ) -> Result<(), mpsc::error::SendError<UpstreamCommand>> {
        self.upstream_command_tx
            .send(UpstreamCommand::MergePeerList { ips })
            .await?;
        Ok(())
    }

    pub async fn get_advertisable_peer_list(&mut self) -> BoxResult<Vec<IpAddr>> {
        let (response_tx, response_rx) = oneshot::channel::<Vec<IpAddr>>();
        self.upstream_command_tx
            .send(UpstreamCommand::GetAdvertisablePeerList { response_tx })
            .await?;
        Ok(response_rx.await?)
    }

    pub async fn peer_closed(
        &mut self,
        ip: IpAddr,
        reason: PeerClosureReason,
    ) -> Result<(), mpsc::error::SendError<UpstreamCommand>> {
        self.upstream_command_tx
            .send(UpstreamCommand::PeerClosed { ip, reason })
            .await?;
        Ok(())
    }

    pub async fn peer_alive(
        &mut self,
        ip: IpAddr,
    ) -> Result<(), mpsc::error::SendError<UpstreamCommand>> {
        self.upstream_command_tx
            .send(UpstreamCommand::PeerAlive { ip })
            .await?;
        Ok(())
    }
}

async fn controller_fn(
    cfg: NetworkConfig,
    mut peer_db: PeerDatabase,
    mut stop_rx: oneshot::Receiver<()>,
    mut upstream_command_rx: mpsc::Receiver<UpstreamCommand>,
    mut event_tx: mpsc::Sender<NetworkControllerEvent>,
) {
    let listen_addr = cfg.bind;

    // prepare connectors
    let mut connectors = FuturesUnordered::new();

    // launch listener
    let (listener_stop_tx, listener_stop_rx) = oneshot::channel::<()>();
    let (listener_socket_tx, mut listener_socket_rx) = mpsc::channel::<(IpAddr, TcpStream)>(1024);
    let listener_handle = tokio::spawn(async move {
        listener_fn(listen_addr, listener_stop_rx, listener_socket_tx).await;
    });

    loop {
        peer_db.cleanup(cfg.max_idle_peers, cfg.max_banned_peers); // removes dead connections
        peer_db.save();

        {
            // try to connect to candidate IPs
            let connector_candidate_ips = peer_db.get_connector_candidate_ips(
                cfg.target_outgoing_connections,
                cfg.max_simultaneous_outgoing_connection_attempts,
            );
            for ip in connector_candidate_ips {
                peer_db
                    .peers
                    .get_mut(&ip)
                    .expect("trying to connect to an unkonwn peer")
                    .status = PeerStatus::OutConnecting;
                connectors.push(connector_fn(SocketAddr::new(ip, listen_addr.port())));
            }
        }

        tokio::select! {
            // peer feedback event
            res = upstream_command_rx.next() => match res {
                Some(UpstreamCommand::MergePeerList{ips}) => {
                    peer_db.merge_candidate_peers(&ips);
                },
                Some(UpstreamCommand::GetAdvertisablePeerList{response_tx}) => {
                    let mut result = peer_db.get_advertisable_peer_ips();
                    if let Some(routable_ip) = cfg.routable_ip {
                        result.insert(0, routable_ip)
                    }
                    let _ = response_tx.send(result);
                },
                Some(UpstreamCommand::PeerClosed{ip, reason}) => {
                    let mut peer = peer_db.peers.get_mut(&ip).expect("disconnected from an unkonwn peer");
                    match reason {
                        PeerClosureReason::Normal => {
                            peer.status = PeerStatus::Idle;
                            peer.last_alive = Some(Utc::now());
                        },
                        PeerClosureReason::ConnectionFailed => {
                            peer.status = PeerStatus::Idle;
                            peer.last_failure = Some(Utc::now());
                        },
                        PeerClosureReason::Banned => {
                            peer.status = PeerStatus::Banned;
                            peer.last_failure = Some(Utc::now());
                        }
                    }
                },
                Some(UpstreamCommand::PeerAlive { ip } ) => {
                    let mut peer = peer_db.peers.get_mut(&ip).expect("conection OK from an unkonwn peer");
                    peer.status = match peer.status {
                        PeerStatus::InHandshaking => PeerStatus::InAlive,
                        PeerStatus::OutHandshaking => PeerStatus::OutAlive,
                        _ => unreachable!("connection OK from peer that was not in the process of connecting")
                    };
                    peer.last_alive = Some(Utc::now());
                }
                None => unreachable!("peer feedback channel disappeared"),
            },

            // connector event
            Some((ip_addr, res)) = connectors.next() => {
                let peer = match peer_db.peers.get_mut(&ip_addr) {
                    Some(p) => match p.status {
                        PeerStatus::OutConnecting => p,
                        _ => continue  // not in OutConnecting status (avoid double-connection)
                    },
                    _ => continue  // not in known peer list
                };
                match res {
                    Ok(socket) => {
                        peer.status = PeerStatus::OutHandshaking;
                        if event_tx.send(NetworkControllerEvent::CandidateConnection{
                            ip: ip_addr,
                            socket: socket
                        }).await.is_err() { unreachable!("could not send out-connected peer upstream") }
                    },
                    Err(_) => {
                        peer.status = PeerStatus::Idle;
                        peer.last_failure = Some(Utc::now());
                    },
                }
            },

            // listener event
            res = listener_socket_rx.next() => match res {
                Some((ip_addr, socket)) => {
                    if peer_db.count_peers_with_status(PeerStatus::InHandshaking) >= cfg.max_simultaneous_incoming_connection_attempts { continue }
                    if peer_db.count_peers_with_status(PeerStatus::InAlive) >= cfg.max_incoming_connections { continue }
                    let peer = peer_db.peers.entry(ip_addr).or_insert(PeerInfo {
                        ip: ip_addr,
                        status: PeerStatus::Idle,
                        advertised_as_reachable: false,
                        bootstrap: false,
                        last_alive: None,
                        last_failure: None
                    });
                    match peer.status {
                        PeerStatus::OutConnecting => {}, // override out-connection attempts (but not handshake)
                        PeerStatus::Idle => {},
                        PeerStatus::Banned => {
                            peer.last_failure = Some(Utc::now());  // save latest connection attempt of banned peer
                            continue;
                        },
                        _ => continue, // avoid double-connection and banned
                    }
                    peer.status = PeerStatus::InHandshaking;
                    if event_tx.send(NetworkControllerEvent::CandidateConnection{
                        ip: ip_addr,
                        socket: socket
                    }).await.is_err() { unreachable!("could not send in-connected peer upstream") }
                },
                None => unreachable!("listener disappeared"),
            },

            // stop message
            _ = &mut stop_rx => break,
        }
    }

    // stop listener
    listener_socket_rx.close();
    let _ = listener_stop_tx.send(());
    let _ = listener_handle.await;

    // wait for connectors to finish
    while let Some(_) = connectors.next().await {}

    // stop peer db
    peer_db.cleanup(cfg.max_idle_peers, cfg.max_banned_peers); // removes dead connections
    peer_db.stop().await;
}

pub async fn listener_fn(
    addr: SocketAddr,
    mut stop_rx: oneshot::Receiver<()>,
    mut socket_tx: mpsc::Sender<(IpAddr, TcpStream)>,
) {
    'reset_loop: loop {
        if let Err(oneshot::error::TryRecvError::Empty) = stop_rx.try_recv() {
        } else {
            break 'reset_loop;
        }
        let mut listener = tokio::select! {
            res = TcpListener::bind(addr) => match res {
                Ok(v) => v,
                Err(e) => {
                    warn!("network listener bind error: {}", e);
                    sleep(Duration::from_secs(1)).await;
                    continue 'reset_loop;
                },
            },
            _ = &mut stop_rx => break 'reset_loop,
        };
        loop {
            tokio::select! {
                res = listener.accept() => match res {
                    Ok((socket, remote_addr)) => {
                        if socket_tx.send((remote_addr.ip(), socket)).await.is_err() {
                            break 'reset_loop;
                        }
                    },
                    Err(_) => {},
                },
                _ = &mut stop_rx => break 'reset_loop,
            }
        }
    }
}

pub async fn connector_fn(addr: SocketAddr) -> (IpAddr, BoxResult<TcpStream>) {
    match tokio::net::TcpStream::connect(addr).await {
        Ok(socket) => (addr.ip(), Ok(socket)),
        Err(e) => (addr.ip(), Err(Box::new(e))),
    }
}
