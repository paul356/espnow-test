use esp_idf_svc::{
    espnow::{EspNow, PeerInfo, BROADCAST},
    eventloop::EspSystemEventLoop,
    hal::peripherals::Peripherals,
    nvs::EspDefaultNvsPartition,
    sys::EspError,
    wifi::{EspWifi, WifiDeviceId},
};
use heapless::FnvIndexMap;
use serde::{Deserialize, Serialize};
use std::{
    sync::{mpsc, Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

// Constants
const MAX_PEERS: usize = 16;
const DEFAULT_TTL: u32 = 30; // seconds
const DISCOVER_INTERVAL: Duration = Duration::from_secs(5);
const LIVENESS_INTERVAL: Duration = Duration::from_secs(10);
const TTL_UPDATE_INTERVAL: Duration = Duration::from_secs(1);
const MESSAGE_BUFFER_SIZE: usize = 250;

// Message types
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MessageType {
    DiscoverServices,
    PeerInfo {
        peers: Vec<([u8; 6], u32)>, // (MAC, TTL) pairs
    },
    Live,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub msg_type: MessageType,
    pub sender_mac: [u8; 6],
}

// Peer information with TTL
#[derive(Debug, Clone)]
pub struct PeerEntry {
    pub mac: [u8; 6],
    pub ttl: u32,
    pub last_seen: Instant,
}

// Shared state for the ESP-NOW system
#[derive(Debug)]
pub struct PeerState {
    pub peers: FnvIndexMap<[u8; 6], PeerEntry, MAX_PEERS>,
    pub own_mac: [u8; 6],
    pub last_discover: Option<Instant>,
    pub last_liveness: Option<Instant>,
    pub last_peer_info_send: FnvIndexMap<[u8; 6], Instant, MAX_PEERS>,
}

impl PeerState {
    pub fn new(own_mac: [u8; 6]) -> Self {
        Self {
            peers: FnvIndexMap::new(),
            own_mac,
            last_discover: None,
            last_liveness: None,
            last_peer_info_send: FnvIndexMap::new(),
        }
    }

    pub fn add_or_update_peer(&mut self, mac: [u8; 6], ttl: u32, espnow: &Arc<Mutex<EspNow<'static>>>) {
        if mac != self.own_mac {
            let peer_entry = PeerEntry {
                mac,
                ttl,
                last_seen: Instant::now(),
            };
            if !self.peers.contains_key(&mac) {
                // Add broadcast peer
                let peer_info = PeerInfo {
                    peer_addr: mac,
                    ..Default::default()
                };
                espnow.lock().unwrap().add_peer(peer_info);
                log::info!("ESP-NOW initialized with broadcast peer");
            }
            self.peers.insert(mac, peer_entry).ok();
            log::info!("Added/Updated peer: {:02X?} with TTL: {}", mac, ttl);
        }
    }

    pub fn update_ttls(&mut self, espnow: &mut EspNow) {
        let mut to_remove = heapless::Vec::<[u8; 6], MAX_PEERS>::new();

        for (mac, peer) in self.peers.iter_mut() {
            if peer.ttl > 0 {
                peer.ttl -= 1;
            }
            if peer.ttl == 0 {
                to_remove.push(*mac).ok();
            }
        }

        for mac in to_remove.iter() {
            self.peers.remove(mac);
            // Remove the peer from ESP-NOW when it expires

            if let Err(e) = espnow.del_peer(*mac) {
                log::error!(
                    "Failed to remove peer {} from ESP-NOW: {:?}",
                    mac_to_string(mac),
                    e
                );
            } else {
                log::info!("Removed peer from ESP-NOW: {}", mac_to_string(mac));
            }

            log::info!("Removed expired peer: {:02X?}", mac);
        }
    }

    pub fn get_peer_list(&self) -> Vec<([u8; 6], u32)> {
        self.peers
            .iter()
            .map(|(mac, peer)| (*mac, peer.ttl))
            .collect()
    }

    pub fn has_peers(&self) -> bool {
        !self.peers.is_empty()
    }
}

fn mac_to_string(mac: &[u8; 6]) -> String {
    format!(
        "{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

fn send_message(espnow: &mut EspNow, target: [u8; 6], message: &Message) -> Result<(), EspError> {
    let serialized = serde_json::to_vec(message)
        .map_err(|_| EspError::from_infallible::<{ esp_idf_svc::sys::ESP_ERR_INVALID_ARG }>())?;

    if serialized.len() > MESSAGE_BUFFER_SIZE {
        log::warn!("Message too large: {} bytes", serialized.len());
        return Err(EspError::from_infallible::<
            { esp_idf_svc::sys::ESP_ERR_INVALID_SIZE },
        >());
    }

    espnow.send(target, &serialized)?;
    log::debug!(
        "Sent message to {}: {:?}",
        mac_to_string(&target),
        message.msg_type
    );
    Ok(())
}

// Message Action enum for the channel
#[derive(Debug, Clone)]
enum MessageAction {
    SendPeerInfo { target: [u8; 6] },
}

fn handle_received_message_callback(
    state: &Arc<Mutex<PeerState>>,
    espnow: &Arc<Mutex<EspNow<'static>>>,
    sender_mac: [u8; 6],
    data: &[u8],
    sender: &mpsc::Sender<MessageAction>,
) -> Result<(), Box<dyn std::error::Error>> {
    let message: Message = serde_json::from_slice(data)?;

    log::info!(
        "Received message from {}: {:?}",
        mac_to_string(&sender_mac),
        message.msg_type
    );

    let mut state_guard = state.lock().unwrap();

    match message.msg_type {
        MessageType::DiscoverServices => {
            // Add the discovering peer and note that we need to respond
            state_guard.add_or_update_peer(sender_mac, DEFAULT_TTL, espnow);

            // Send PeerInfo action through channel
            if let Err(e) = sender.send(MessageAction::SendPeerInfo { target: sender_mac }) {
                log::error!("Failed to send peer info action: {:?}", e);
            }

            log::info!(
                "Discovery request from {}, will send peer info",
                mac_to_string(&sender_mac)
            );
        }
        MessageType::PeerInfo { peers } => {
            // Update our peer list with received information
            for (peer_mac, ttl) in peers.iter() {
                state_guard.add_or_update_peer(*peer_mac, *ttl, espnow);
            }
        }
        MessageType::Live => {
            // Update or add peer to the peer list
            state_guard.add_or_update_peer(sender_mac, DEFAULT_TTL, espnow);
        }
    }

    Ok(())
}

fn network_task(
    state: Arc<Mutex<PeerState>>,
    espnow: Arc<Mutex<EspNow<'static>>>,
    receiver: mpsc::Receiver<MessageAction>,
) {
    loop {
        // Handle any pending message actions
        while let Ok(action) = receiver.try_recv() {
            match action {
                MessageAction::SendPeerInfo { target } => {
                    let peer_list = {
                        let state_guard = state.lock().unwrap();
                        state_guard.get_peer_list()
                    };

                    if let Ok(mut espnow_guard) = espnow.lock() {
                        let message = Message {
                            msg_type: MessageType::PeerInfo { peers: peer_list },
                            sender_mac: state.lock().unwrap().own_mac,
                        };

                        if let Err(e) = send_message(&mut espnow_guard, target, &message) {
                            log::error!(
                                "Failed to send peer info to {}: {:?}",
                                mac_to_string(&target),
                                e
                            );
                        } else {
                            log::info!("Sent peer info to {}", mac_to_string(&target));
                            // Update the last_peer_info_send timestamp
                            if let Ok(mut state_guard) = state.lock() {
                                state_guard
                                    .last_peer_info_send
                                    .insert(target, Instant::now())
                                    .ok();
                            }
                        }
                    }
                }
            }
        }

        // Check for discovery/liveness interval
        {
            let mut state_guard = state.lock().unwrap();
            let should_discover = if state_guard.peers.is_empty() {
                if let Some(last_discover) = state_guard.last_discover {
                    last_discover.elapsed() >= DISCOVER_INTERVAL
                } else {
                    true
                }
            } else {
                false
            };

            let should_send_liveness = if state_guard.peers.is_empty() {
                false
            } else {
                if let Some(last_liveness) = state_guard.last_liveness {
                    last_liveness.elapsed() >= LIVENESS_INTERVAL
                } else {
                    true
                }
            };

            if should_discover || should_send_liveness {
                let message_type = if should_discover {
                    state_guard.last_discover = Some(Instant::now());
                    MessageType::DiscoverServices
                } else {
                    state_guard.last_liveness = Some(Instant::now());
                    MessageType::Live
                };

                let message = Message {
                    msg_type: message_type,
                    sender_mac: state_guard.own_mac,
                };

                drop(state_guard); // Release lock before sending

                if let Ok(mut espnow_guard) = espnow.lock() {
                    // Always send to broadcast address
                    if let Err(e) = send_message(&mut espnow_guard, BROADCAST, &message) {
                        log::error!("Failed to send message: {:?}", e);
                    } else {
                        log::info!("Sent {:?} broadcast", message.msg_type);
                    }
                }
            }
        }

        thread::sleep(Duration::from_millis(100));
    }
}

fn ttl_update_task(state: Arc<Mutex<PeerState>>, espnow: Arc<Mutex<EspNow<'static>>>) {
    loop {
        thread::sleep(TTL_UPDATE_INTERVAL);

        if let Ok(mut state_guard) = state.lock() {
            if let Ok(mut espnow_guard) = espnow.lock() {
                state_guard.update_ttls(&mut espnow_guard);
            }
        }
    }
}

fn main() -> anyhow::Result<()> {
    // It is necessary to call this function once. Otherwise some patches to the runtime
    // implemented by esp-idf-sys might not link properly. See https://github.com/esp-rs/esp-idf-template/issues/71
    esp_idf_svc::sys::link_patches();

    // Bind the log crate to the ESP Logging facilities
    esp_idf_svc::log::EspLogger::initialize_default();

    log::info!("Starting ESP-NOW peer discovery system");

    // Initialize peripherals
    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // Initialize WiFi driver
    let mut wifi = EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs))?;
    wifi.set_configuration(&esp_idf_svc::wifi::Configuration::Client(
        esp_idf_svc::wifi::ClientConfiguration::default(),
    ))?;
    wifi.start()?;

    // Get our MAC address
    let own_mac = wifi.driver().get_mac(WifiDeviceId::Sta)?;
    log::info!("Our MAC address: {}", mac_to_string(&own_mac));

    // Initialize ESP-NOW
    let espnow = EspNow::take()?;

    // Add broadcast peer
    let broadcast_peer = PeerInfo {
        peer_addr: BROADCAST,
        ..Default::default()
    };
    espnow.add_peer(broadcast_peer)?;
    log::info!("ESP-NOW initialized with broadcast peer");

    // Create shared state
    let state = Arc::new(Mutex::new(PeerState::new(own_mac)));

    // Create a channel for message actions
    let (sender, receiver) = mpsc::channel::<MessageAction>();


    let espnow_shared = Arc::new(Mutex::new(espnow));

    // Register receive callback
    let state_for_callback = Arc::clone(&state);
    let espnow_for_callback = Arc::clone(&espnow_shared);
    let sender_for_callback = sender.clone();
    espnow_shared.lock().unwrap().register_recv_cb(move |receive_info, data| {
        let sender_mac = *receive_info.src_addr;
        if let Err(e) = handle_received_message_callback(
            &state_for_callback,
            &espnow_for_callback,
            sender_mac,
            data,
            &sender_for_callback,
        ) {
            log::error!("Error handling received message: {:?}", e);
        }
    })?;

    // Clone references for tasks
    let state_network = Arc::clone(&state);
    let state_ttl = Arc::clone(&state);
    let espnow_network = Arc::clone(&espnow_shared);
    let espnow_ttl = Arc::clone(&espnow_shared);

    // Start background tasks
    let _network_handle = thread::spawn(move || {
        network_task(state_network, espnow_network, receiver);
    });

    let _ttl_handle = thread::spawn(move || {
        ttl_update_task(state_ttl, espnow_ttl);
    });

    // Main status printing loop
    log::info!("Starting main loop");

    loop {
        // Print peer status periodically
        static mut LAST_STATUS_PRINT: Option<Instant> = None;
        unsafe {
            let should_print = if let Some(last_print) = LAST_STATUS_PRINT {
                last_print.elapsed() >= Duration::from_secs(30)
            } else {
                true
            };

            if should_print {
                let state_guard = state.lock().unwrap();
                log::info!("Current peers ({}): ", state_guard.peers.len());
                for (mac, peer) in state_guard.peers.iter() {
                    log::info!("  {} - TTL: {}", mac_to_string(mac), peer.ttl);
                }
                LAST_STATUS_PRINT = Some(Instant::now());
            }
        }

        thread::sleep(Duration::from_secs(5));
    }
}
