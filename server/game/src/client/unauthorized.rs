use std::{
    net::SocketAddrV4,
    sync::atomic::{AtomicI32, AtomicU16, AtomicU32, Ordering},
    time::Duration,
};

use globed_shared::{
    info,
    rand::{self, Rng},
    warn, SyncMutex, UserEntry, PROTOCOL_VERSION,
};
use tokio::{net::TcpStream, sync::Notify};

use super::*;
use crate::{data::*, managers::ComputedRole, server::GameServer, util::LockfreeMutCell};

/// `UnauthorizedThread` is a thread that can be formed for 2 reasons:
/// 1. Initial connection (when a client initiates a TCP connection, an `UnauthorizedThread` is created)
/// 2. Abrupt disconnect (a `ClientThread` will be downgraded to a `UnauthorizedThread` and can be reclaimed for some time)
///
/// In the first mode, two things can happen:
/// 1. Handshake -> `LoginPacket` -> `ClaimThreadPacket` -> thread gets upgraded to a `ClientThread`
/// 2. `LoginRecoverPacket` -> merge with the found `UnauthorizedThread` -> `ClaimThreadPacket` -> thread gets upgraded
///
/// In the second mode, the server waits for someone to try and recover this thread, while it's in `Disconnected` state.
pub struct UnauthorizedThread {
    pub game_server: &'static GameServer,
    pub socket: LockfreeMutCell<ClientSocket>,
    pub connection_state: AtomicClientThreadState,

    pub secret_key: u32,

    pub account_id: AtomicI32,
    pub level_id: AtomicLevelId,
    pub room_id: AtomicU32,

    pub account_data: SyncMutex<PlayerAccountData>,
    pub user_entry: SyncMutex<Option<UserEntry>>,
    pub user_role: SyncMutex<Option<ComputedRole>>,

    pub fragmentation_limit: AtomicU16,

    pub claim_udp_peer: SyncMutex<Option<SocketAddrV4>>,
    pub claim_udp_notify: Notify,
}

pub enum UnauthorizedThreadOutcome {
    Upgrade,
    Terminate,
}

impl UnauthorizedThread {
    pub fn new(socket: TcpStream, peer: SocketAddrV4, game_server: &'static GameServer) -> Self {
        Self {
            game_server,
            socket: LockfreeMutCell::new(ClientSocket::new(socket, peer, game_server)),
            connection_state: AtomicClientThreadState::default(),

            secret_key: rand::thread_rng().gen(),

            account_id: AtomicI32::new(0),
            level_id: AtomicLevelId::new(0),
            room_id: AtomicU32::new(0),

            account_data: SyncMutex::new(PlayerAccountData::default()),
            user_entry: SyncMutex::new(None),
            user_role: SyncMutex::new(None),

            fragmentation_limit: AtomicU16::new(0),

            claim_udp_peer: SyncMutex::new(None),
            claim_udp_notify: Notify::new(),
        }
    }

    /// Returns whether the thread should be upgraded.
    pub async fn run(&self) -> UnauthorizedThreadOutcome {
        loop {
            let state = self.connection_state.load();

            match state {
                ClientThreadState::Established => break UnauthorizedThreadOutcome::Upgrade,
                ClientThreadState::Terminating => break UnauthorizedThreadOutcome::Terminate,
                ClientThreadState::Disconnected => unreachable!("disconnected??"),
                ClientThreadState::Unauthorized | ClientThreadState::Unclaimed => {}
            }

            tokio::select! {
                () = self.wait_for_claimed() => {
                    // we just got claimed by a udp thread and can successfully terminate
                    self.connection_state.store(ClientThreadState::Established);
                },

                result = unsafe { self.socket.get_mut() }.poll_for_tcp_data() => match result {
                    Ok(datalen) => {
                        match self.recv_and_handle(datalen).await {
                            Ok(()) => {},
                            Err(e) => warn!("error on an unauth thread: {e}"),
                        }
                    },

                    Err(err) => {
                        // terminate, an error occurred
                        warn!("error on an unauth thread, terminating: {err}");
                        self.terminate();
                    },
                },

                () = tokio::time::sleep(Duration::from_secs(90)) => {
                    // terminate, no activity for 90+ seconds
                    self.terminate();
                }
            }
        }
    }

    pub fn claim(&self, udp_peer: SocketAddrV4) {
        *self.claim_udp_peer.lock() = Some(udp_peer);
        self.claim_udp_notify.notify_one();
    }

    #[inline]
    async fn recv_and_handle(&self, message_size: usize) -> Result<()> {
        // safety: only we can receive data from our client.
        let socket = unsafe { self.socket.get_mut() };
        socket.recv_and_handle(message_size, async |buf| self.handle_packet(buf).await).await
    }

    async fn handle_packet(&self, message: &mut [u8]) -> Result<()> {
        #[cfg(debug_assertions)]
        if message.len() < PacketHeader::SIZE {
            return Err(PacketHandlingError::MalformedMessage);
        }

        let mut data = ByteReader::from_bytes(message);
        let header = data.read_packet_header()?;

        // reject cleartext credentials
        if header.packet_id == LoginPacket::PACKET_ID && !header.encrypted {
            return Err(PacketHandlingError::MalformedLoginAttempt);
        }

        // decrypt the packet in-place if encrypted
        if header.encrypted {
            data = unsafe { self.socket.get_mut() }.decrypt(message)?;
        }

        match header.packet_id {
            CryptoHandshakeStartPacket::PACKET_ID => self.handle_crypto_handshake(&mut data).await,
            LoginPacket::PACKET_ID => self.handle_login(&mut data).await,
            x => Err(PacketHandlingError::NoHandler(x)),
        }
    }

    // packet handlers

    gs_handler!(self, handle_crypto_handshake, CryptoHandshakeStartPacket, packet, {
        let socket = unsafe { self.socket.get_mut() };

        if packet.protocol != PROTOCOL_VERSION && packet.protocol != 0xffff {
            self.terminate();

            socket.send_packet_static(&ProtocolMismatchPacket { protocol: PROTOCOL_VERSION }).await?;

            return Ok(());
        }

        socket.init_crypto_box(&packet.key)?;
        socket
            .send_packet_static(&CryptoHandshakeResponsePacket {
                key: self.game_server.public_key.clone().into(),
            })
            .await
    });

    gs_handler!(self, handle_login, LoginPacket, packet, {
        // preemptively set the status to terminating, in case anything fails later.
        // if login was successful, change the status back at the end of the method body.
        self.terminate();

        let socket = unsafe { self.socket.get_mut() };

        // disconnect if server is under maintenance
        if self.game_server.bridge.central_conf.lock().maintenance {
            gs_disconnect!(self, "The server is currently under maintenance, please try connecting again later.");
        }

        if packet.fragmentation_limit < 1300 {
            gs_disconnect!(
                self,
                &format!(
                    "The client fragmentation limit is too low ({} bytes) to be accepted",
                    packet.fragmentation_limit
                )
            );
        }

        self.fragmentation_limit.store(packet.fragmentation_limit, Ordering::Relaxed);

        if packet.account_id <= 0 || packet.user_id <= 0 {
            let message = format!(
                "Invalid account/user ID was sent ({} and {}). Please note that you must be signed into a Geometry Dash account before connecting.",
                packet.account_id, packet.user_id
            );
            socket.send_packet_dynamic(&LoginFailedPacket { message: &message }).await?;
            return Ok(());
        }

        // skip authentication if standalone
        let standalone = self.game_server.standalone;
        let player_name = if standalone {
            packet.name
        } else {
            // lets verify the given token
            let result = {
                self.game_server
                    .bridge
                    .token_issuer
                    .lock()
                    .validate(packet.account_id, packet.user_id, packet.token.to_str().unwrap())
            };

            match result {
                Ok(x) => InlineString::new(&x),
                Err(err) => {
                    let mut message = FastString::new("authentication failed: ");
                    message.extend(err.error_message());

                    socket.send_packet_dynamic(&LoginFailedPacket { message: &message }).await?;
                    return Ok(());
                }
            }
        };

        // check if the user is already logged in, kick the other instance
        self.game_server.check_already_logged_in(packet.account_id).await?;

        // fetch data from the central
        if !standalone {
            let user_entry = match self.game_server.bridge.get_user_data(&packet.account_id.to_string()).await {
                Ok(user) if user.is_banned => {
                    socket
                        .send_packet_dynamic(&ServerBannedPacket {
                            message: FastString::new(&user.violation_reason.as_ref().map_or_else(|| "No reason given".to_owned(), |x| x.clone())),
                            timestamp: user.violation_expiry.unwrap(), // TODO: fix
                        })
                        .await?;

                    return Ok(());
                }
                Ok(user) if self.game_server.bridge.is_whitelist() && !user.is_whitelisted => {
                    socket
                        .send_packet_dynamic(&LoginFailedPacket {
                            message: "This server has whitelist enabled and your account has not been allowed.",
                        })
                        .await?;

                    return Ok(());
                }
                Ok(user) => user,
                Err(err) => {
                    let mut message = InlineString::<256>::new("failed to fetch user data: ");
                    message.extend_safe(&err.to_string());

                    socket.send_packet_dynamic(&LoginFailedPacket { message: &message }).await?;
                    return Ok(());
                }
            };

            *self.user_role.lock() = Some(self.game_server.state.role_manager.compute(&user_entry.user_roles));
            *self.user_entry.lock() = Some(user_entry);
        }

        self.account_id.store(packet.account_id, Ordering::Relaxed);
        self.game_server.state.player_count.fetch_add(1u32, Ordering::Relaxed); // increment player count

        info!(
            "[{} ({}) @ {}] Login successful, platform: {}",
            player_name,
            packet.account_id,
            self.get_tcp_peer(),
            packet.platform
        );

        let special_user_data = {
            let mut account_data = self.account_data.lock();
            account_data.account_id = packet.account_id;
            account_data.user_id = packet.user_id;
            account_data.name = player_name;
            account_data.icons.clone_from(&packet.icons);

            let user_entry = self.user_entry.lock();
            let sud = SpecialUserData::from_user_entry(user_entry.as_ref().unwrap(), &self.game_server.state.role_manager);

            account_data.special_user_data.clone_from(&sud);

            sud
        };

        // add them to the global room
        self.game_server.state.room_manager.get_global().manager.create_player(packet.account_id);

        let tps = self.game_server.bridge.central_conf.lock().tps;

        let all_roles = self.game_server.state.role_manager.get_all_roles();

        socket
            .send_packet_dynamic(&LoggedInPacket {
                tps,
                special_user_data,
                all_roles,
                secret_key: self.secret_key,
            })
            .await?;

        self.connection_state.store(ClientThreadState::Unclaimed); // as we still need ClaimThreadPacket to arrive

        Ok(())
    });

    /// Blocks until we get notified that we got claimed by a UDP socket.
    async fn wait_for_claimed(&self) {
        {
            let mut p = self.claim_udp_peer.lock();
            if p.is_some() {
                unsafe { self.socket.get_mut() }.udp_peer = std::mem::take(&mut *p);
                return;
            }
        }

        loop {
            self.claim_udp_notify.notified().await;

            let mut p = self.claim_udp_peer.lock();
            if p.is_some() {
                unsafe { self.socket.get_mut() }.udp_peer = std::mem::take(&mut *p);
                return;
            }
        }
    }

    fn terminate(&self) {
        self.connection_state.store(ClientThreadState::Terminating);
    }

    /// get the tcp address of the connected peer. do not call this from another clientthread
    fn get_tcp_peer(&self) -> SocketAddrV4 {
        unsafe { self.socket.get() }.tcp_peer
    }

    /// terminate and send a message to the user with the reason
    async fn kick(&self, message: &str) -> Result<()> {
        self.terminate();
        unsafe { self.socket.get_mut() }
            .send_packet_dynamic(&ServerDisconnectPacket { message })
            .await
    }

    pub fn upgrade(self) -> ClientThread {
        // make a couple of assertions that must always hold true before upgrading

        debug_assert!(unsafe { self.socket.get() }.udp_peer.is_some(), "udp peer is None when upgrading thread");
        debug_assert!(
            self.account_id.load(std::sync::atomic::Ordering::Relaxed) != 0,
            "account ID is 0 when upgrading thread"
        );
        debug_assert!(self.user_entry.lock().is_some(), "account data is None when upgrading thread");
        debug_assert!(self.user_role.lock().is_some(), "account data is None when upgrading thread");
        debug_assert!(
            self.connection_state.load() == ClientThreadState::Established,
            "connection state is not Established when upgrading thread"
        );

        ClientThread::from_unauthorized(self)
    }
}
