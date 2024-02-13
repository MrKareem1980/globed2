use std::{
    collections::VecDeque,
    net::{SocketAddr, SocketAddrV4},
    sync::{atomic::Ordering, Arc},
    time::Duration,
};

use globed_shared::{
    anyhow::{self, anyhow, bail},
    crypto_box::{aead::OsRng, PublicKey, SecretKey},
    esp::ByteBufferExtWrite as _,
    logger::*,
    GameServerBootData, SyncMutex, TokenIssuer, SERVER_MAGIC_LEN,
};
use rustc_hash::FxHashMap;

#[allow(unused_imports)]
use tokio::sync::oneshot; // no way

use tokio::net::{TcpListener, UdpSocket};

use crate::{
    data::*,
    server_thread::{GameServerThread, ServerThreadMessage, INLINE_BUFFER_SIZE},
    state::ServerState,
};

const MAX_UDP_PACKET_SIZE: usize = 2048;

pub struct GameServerConfiguration {
    pub http_client: reqwest::Client,
    pub central_url: String,
    pub central_pw: String,
}

pub struct GameServer {
    pub state: ServerState,
    pub tcp_socket: TcpListener,
    pub udp_socket: UdpSocket,
    pub threads: SyncMutex<FxHashMap<SocketAddrV4, Arc<GameServerThread>>>,
    pub unclaimed_threads: SyncMutex<VecDeque<Arc<GameServerThread>>>,
    pub secret_key: SecretKey,
    pub public_key: PublicKey,
    pub central_conf: SyncMutex<GameServerBootData>,
    pub config: GameServerConfiguration,
    pub standalone: bool,
    pub token_issuer: TokenIssuer,
}

impl GameServer {
    pub fn new(
        tcp_socket: TcpListener,
        udp_socket: UdpSocket,
        state: ServerState,
        central_conf: GameServerBootData,
        config: GameServerConfiguration,
        standalone: bool,
    ) -> Self {
        let secret_key = SecretKey::generate(&mut OsRng);
        let public_key = secret_key.public_key();
        let token_issuer = TokenIssuer::new(&central_conf.secret_key2, Duration::from_secs(central_conf.token_expiry));

        Self {
            state,
            tcp_socket,
            udp_socket,
            threads: SyncMutex::new(FxHashMap::default()),
            unclaimed_threads: SyncMutex::new(VecDeque::new()),
            secret_key,
            public_key,
            central_conf: SyncMutex::new(central_conf),
            config,
            standalone,
            token_issuer,
        }
    }

    pub async fn run(&'static self) -> ! {
        info!("Server launched on {}", self.tcp_socket.local_addr().unwrap());

        // spawn central conf refresher (runs every 5 minutes)
        if !self.standalone {
            tokio::spawn(async {
                let mut interval = tokio::time::interval(Duration::from_secs(300));
                interval.tick().await;

                loop {
                    interval.tick().await;
                    match self.refresh_bootdata().await {
                        Ok(()) => debug!("refreshed central server configuration"),
                        Err(e) => error!("failed to refresh configuration from the central server: {e}"),
                    }
                }
            });
        }

        // print some useful stats every once in a bit
        let interval = self.central_conf.lock().status_print_interval;

        if interval != 0 {
            tokio::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(interval));
                interval.tick().await;

                loop {
                    interval.tick().await;
                    self.print_server_status();
                }
            });
        }

        // spawn the udp packet handler

        tokio::spawn(async move {
            let mut buf = [0u8; MAX_UDP_PACKET_SIZE];

            loop {
                match self.recv_and_handle_udp(&mut buf).await {
                    Ok(()) => {}
                    Err(e) => {
                        warn!("failed to handle udp packet: {e}");
                    }
                }
            }
        });

        loop {
            match self.accept_connection().await {
                Ok(()) => {}
                Err(err) => {
                    error!("Failed to accept a connection: {err}");
                }
            }
        }
    }

    async fn accept_connection(&'static self) -> anyhow::Result<()> {
        let (socket, peer) = self.tcp_socket.accept().await?;

        let peer = match peer {
            SocketAddr::V4(x) => x,
            SocketAddr::V6(_) => bail!("rejecting request from ipv6 host"),
        };

        debug!(
            "accepting tcp connection from {peer}, thread count: {}, unclaimed: {}",
            self.threads.lock().len(),
            self.unclaimed_threads.lock().len()
        );

        let thread = Arc::new(GameServerThread::new(socket, peer, self));
        self.unclaimed_threads.lock().push_back(thread.clone());

        tokio::spawn(async move {
            // `thread.run()` will return in either one of those 3 conditions:
            // 1. no messages sent by the peer for 60 seconds
            // 2. the channel was closed (normally impossible for that to happen)
            // 3. `thread.terminate()` was called on that thread (due to a disconnect from either side)
            // additionally, if it panics then the state of the player will be frozen forever,
            // they won't be removed from levels or the player count and that person has to restart the game to connect again.
            // so try to avoid panics please..
            thread.run().await;
            trace!("removing client: {}", peer);
            self.post_disconnect_cleanup(&thread, peer);

            // if any thread was waiting for us to terminate, tell them it's finally time.l
            thread.cleanup_notify.notify_waiters();
        });

        Ok(())
    }

    async fn recv_and_handle_udp(&'static self, buf: &mut [u8]) -> anyhow::Result<()> {
        let (len, peer) = self.udp_socket.recv_from(buf).await?;

        let peer = match peer {
            SocketAddr::V4(x) => x,
            SocketAddr::V6(_) => bail!("rejecting request from ipv6 host"),
        };

        // if it's a ping packet, we can handle it here. otherwise we send it to the appropriate thread.
        if !self.try_udp_handle(&buf[..len], peer).await? {
            let thread = { self.threads.lock().get(&peer).cloned() };
            if let Some(thread) = thread {
                thread
                    .push_new_message(if len <= INLINE_BUFFER_SIZE {
                        let mut inline_buf = [0u8; INLINE_BUFFER_SIZE];
                        inline_buf[..len].clone_from_slice(&buf[..len]);

                        ServerThreadMessage::SmallPacket((inline_buf, len))
                    } else {
                        ServerThreadMessage::Packet(buf[..len].to_vec())
                    })
                    .await;
            }
        }

        Ok(())
    }

    /* various calls for other threads */

    pub fn claim_thread(&'static self, udp_addr: SocketAddrV4, secret_key: u32) {
        let mut unclaimed = self.unclaimed_threads.lock();
        let idx = unclaimed.iter().position(|thr| {
            thr.claim_secret_key.load(Ordering::Relaxed) == secret_key && !thr.claimed.load(Ordering::Relaxed)
        });

        if let Some(idx) = idx {
            if let Some(thread) = unclaimed.remove(idx) {
                *thread.udp_peer.lock() = udp_addr;
                thread.claimed.store(true, Ordering::Relaxed);
                self.threads.lock().insert(udp_addr, thread);
            }
        }
    }

    pub async fn broadcast_voice_packet(&'static self, vpkt: &Arc<VoiceBroadcastPacket>, level_id: i32, room_id: u32) {
        self.broadcast_user_message(
            &ServerThreadMessage::BroadcastVoice(vpkt.clone()),
            vpkt.player_id,
            level_id,
            room_id,
        )
        .await;
    }

    pub async fn broadcast_chat_packet(&'static self, tpkt: &ChatMessageBroadcastPacket, level_id: i32, room_id: u32) {
        self.broadcast_user_message(
            &ServerThreadMessage::BroadcastText(tpkt.clone()),
            tpkt.player_id,
            level_id,
            room_id,
        )
        .await;
    }

    /// iterate over every player in this list and run F
    pub fn for_each_player<F, A>(&'static self, ids: &[i32], f: F, additional: &mut A) -> usize
    where
        F: Fn(&PlayerAccountData, usize, &mut A) -> bool,
    {
        self.threads
            .lock()
            .values()
            .filter(|thread| ids.contains(&thread.account_id.load(Ordering::Relaxed)))
            .map(|thread| thread.account_data.lock().clone())
            .fold(0, |count, data| count + usize::from(f(&data, count, additional)))
    }

    /// iterate over every authenticated player and run F
    pub fn for_every_player_preview<F, A>(&'static self, f: F, additional: &mut A) -> usize
    where
        F: Fn(&PlayerPreviewAccountData, usize, &mut A) -> bool,
    {
        self.threads
            .lock()
            .values()
            .filter(|thr| thr.authenticated())
            .map(|thread| thread.account_data.lock().make_preview())
            .fold(0, |count, preview| count + usize::from(f(&preview, count, additional)))
    }

    pub fn for_every_room_player_preview<F, A>(&'static self, room_id: u32, f: F, additional: &mut A) -> usize
    where
        F: Fn(&PlayerRoomPreviewAccountData, usize, &mut A) -> bool,
    {
        self.threads
            .lock()
            .values()
            .filter(|thr| thr.authenticated() && thr.room_id.load(Ordering::Relaxed) == room_id)
            .map(|thread| {
                thread
                    .account_data
                    .lock()
                    .make_room_preview(thread.level_id.load(Ordering::Relaxed))
            })
            .fold(0, |count, preview| count + usize::from(f(&preview, count, additional)))
    }

    pub fn get_player_account_data(&'static self, account_id: i32) -> Option<PlayerAccountData> {
        self.threads
            .lock()
            .values()
            .find(|thr| thr.account_id.load(Ordering::Relaxed) == account_id)
            .map(|thr| thr.account_data.lock().clone())
    }

    pub fn chat_blocked(&'static self, user_id: i32) -> bool {
        self.central_conf.lock().no_chat.contains(&user_id)
    }

    /// If someone is already logged in under the given account ID, logs them out.
    /// Additionally, blocks until the appropriate cleanup has been done.
    pub async fn check_already_logged_in(&'static self, user_id: i32) -> anyhow::Result<()> {
        let thread = self
            .threads
            .lock()
            .values()
            .find(|thr| thr.account_id.load(Ordering::Relaxed) == user_id)
            .cloned();

        if let Some(thread) = thread {
            thread
                .push_new_message(ServerThreadMessage::TerminationNotice(FastString::from_str(
                    "Someone logged into the same account from a different place.",
                )))
                .await;

            let _mtx = thread.cleanup_mutex.lock().await;
            thread.cleanup_notify.notified().await;
        }

        Ok(())
    }

    /// If the passed string is numeric, tries to find a user by account ID, else by their account name.
    pub fn get_user_by_name_or_id(&'static self, name: &str) -> Option<Arc<GameServerThread>> {
        self.threads
            .lock()
            .values()
            .find(|thr| {
                // if it's a valid int, assume it's an account ID
                if let Ok(account_id) = name.parse::<i32>() {
                    thr.account_id.load(Ordering::Relaxed) == account_id
                } else {
                    // else assume it's a player name
                    thr.account_data.lock().name.eq_ignore_ascii_case(name)
                }
            })
            .cloned()
    }
    /* private handling stuff */

    /// broadcast a message to all people on the level
    async fn broadcast_user_message(&'static self, msg: &ServerThreadMessage, origin_id: i32, level_id: i32, room_id: u32) {
        let threads = self.state.room_manager.with_any(room_id, |pm| {
            let players = pm.get_level(level_id);

            if let Some(players) = players {
                self.threads
                    .lock()
                    .values()
                    .filter(|thread| {
                        let account_id = thread.account_id.load(Ordering::Relaxed);
                        account_id != origin_id && players.contains(&account_id)
                    })
                    .cloned()
                    .collect()
            } else {
                Vec::new()
            }
        });

        for thread in threads {
            thread.push_new_message(msg.clone()).await;
        }
    }

    /// Try to handle a packet that is not addresses to a specific thread, but to the game server.
    async fn try_udp_handle(&'static self, data: &[u8], peer: SocketAddrV4) -> anyhow::Result<bool> {
        let mut byte_reader = ByteReader::from_bytes(data);
        let header = byte_reader.read_packet_header().map_err(|e| anyhow!("{e}"))?;

        match header.packet_id {
            PingPacket::PACKET_ID => {
                let pkt = PingPacket::decode_from_reader(&mut byte_reader).map_err(|e| anyhow!("{e}"))?;
                let response = PingResponsePacket {
                    id: pkt.id,
                    player_count: self.state.player_count.load(Ordering::Relaxed),
                };

                let mut buf_array = [0u8; PacketHeader::SIZE + PingResponsePacket::ENCODED_SIZE];
                let mut buf = FastByteBuffer::new(&mut buf_array);
                buf.write_packet_header::<PingResponsePacket>();
                buf.write_value(&response);

                let send_bytes = buf.as_bytes();

                match self.udp_socket.try_send_to(send_bytes, SocketAddr::V4(peer)) {
                    Ok(_) => Ok(true),
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        self.udp_socket.send_to(send_bytes, peer).await?;
                        Ok(true)
                    }
                    Err(e) => Err(e.into()),
                }
            }

            ClaimThreadPacket::PACKET_ID => {
                let pkt = ClaimThreadPacket::decode_from_reader(&mut byte_reader).map_err(|e| anyhow!("{e}"))?;
                self.claim_thread(peer, pkt.secret_key);
                Ok(true)
            }

            _ => Ok(false),
        }
    }

    fn post_disconnect_cleanup(&'static self, thread: &Arc<GameServerThread>, tcp_peer: SocketAddrV4) {
        if thread.claimed.load(Ordering::Relaxed) {
            // self.threads.lock().retain(|_udp_peer, thread| thread.tcp_peer != tcp_peer);
            let mut threads = self.threads.lock();
            let udp_peer = threads
                .iter()
                .find(|(_udp, thread)| thread.tcp_peer == tcp_peer)
                .map(|x| *x.0);

            if let Some(udp_peer) = udp_peer {
                threads.remove(&udp_peer);
            }
        } else {
            let mut unclaimed = self.unclaimed_threads.lock();
            let idx = unclaimed.iter().position(|thr| Arc::ptr_eq(thr, thread));
            if let Some(idx) = idx {
                unclaimed.remove(idx);
            }
        }

        let account_id = thread.account_id.load(Ordering::Relaxed);

        if account_id == 0 {
            return;
        }

        let level_id = thread.level_id.load(Ordering::Relaxed);
        let room_id = thread.room_id.load(Ordering::Relaxed);

        // decrement player count
        self.state.player_count.fetch_sub(1, Ordering::Relaxed);

        // remove from the player manager and the level if they are on one

        self.state.room_manager.with_any(room_id, |pm| {
            pm.remove_player(account_id);

            if level_id != 0 {
                pm.remove_from_level(level_id, account_id);
            }
        });

        if room_id != 0 {
            self.state.room_manager.maybe_remove_room(room_id);
        }
    }

    fn print_server_status(&'static self) {
        info!("Current server stats (printed once an hour)");
        info!(
            "Player threads: {}, player count: {}",
            self.threads.lock().len(),
            self.state.player_count.load(Ordering::Relaxed)
        );
        info!("Amount of rooms: {}", self.state.room_manager.get_rooms().len());
        info!(
            "People in the global room: {}",
            self.state.room_manager.get_global().get_total_player_count()
        );
        info!("-------------------------------------------");
    }

    async fn refresh_bootdata(&'static self) -> anyhow::Result<()> {
        let response = self
            .config
            .http_client
            .post(format!("{}{}", self.config.central_url, "gs/boot"))
            .query(&[("pw", self.config.central_pw.clone())])
            .send()
            .await?
            .error_for_status()
            .map_err(|e| anyhow!("central server returned an error: {e}"))?;

        let configuration = response.bytes().await?;

        let mut reader = ByteReader::from_bytes(&configuration);
        reader.skip(SERVER_MAGIC_LEN);

        let boot_data: GameServerBootData = reader
            .read_value()
            .map_err(|e| anyhow!("central server sent malformed response: {e}"))?;

        let is_now_under_maintenance;
        {
            let mut conf = self.central_conf.lock();
            is_now_under_maintenance = !conf.maintenance && boot_data.maintenance;
            *conf = boot_data;
        }

        // if we are now under maintenance, disconnect everyone who's still connected
        if is_now_under_maintenance {
            let threads: Vec<_> = self.threads.lock().values().cloned().collect();
            for thread in threads {
                thread
                    .push_new_message(ServerThreadMessage::TerminationNotice(FastString::from_str(
                        "The server is now under maintenance, please try connecting again later",
                    )))
                    .await;
            }
        }

        Ok(())
    }
}
