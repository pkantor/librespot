use std::{net::UdpSocket, thread, time::Duration};

use librespot_connect::spirc::Spirc;
use librespot_core::spotify_id::SpotifyItemType;
use librespot_playback::player::PlayerEventChannel;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self};

use log::debug;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ApiServerError {
    
}

enum ApiServerCommand {
    SetSpirc(Spirc),
    Quit,
}

pub struct ApiServerHandler {
    cmd_tx: mpsc::UnboundedSender<ApiServerCommand>,
    task_running: bool,
}

impl ApiServerHandler {
    pub async fn spawn(player_events: PlayerEventChannel) -> Result<ApiServerHandler, ApiServerError> {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let task_running = false;

        let api_server_task = ApiServerTask {
            spirc: None,
            cmd_rx,
            task_running,
            player_events
        };
        println!("pre run");
        api_server_task.run();

        println!("pre ok");
        Ok(ApiServerHandler {
            cmd_tx,
            task_running
        })
    }

    pub fn set_spirc(&self, spirc: Spirc) {
        let _ = self.cmd_tx.send(ApiServerCommand::SetSpirc(spirc));
    }

    pub async fn quit_and_join(self) {
        let _ = self.cmd_tx.send(ApiServerCommand::Quit);
        println!("waiting for task to stop");
        while self.task_running {
            thread::sleep(Duration::from_millis(100));
        }
        println!("APIServer task stopped")
    }
}

struct ApiServerTask {
    spirc: Option<Spirc>,
    cmd_rx: mpsc::UnboundedReceiver<ApiServerCommand>,
    task_running: bool,
    player_events: PlayerEventChannel
}

#[derive(Serialize, Deserialize, Debug)]
struct GetCurrentTrackRes {
    song_name: String,
    song_id: String,
    song_artists: Vec<String>,
    song_uri: String,
}

impl ApiServerTask {

    fn run(mut self) {
        self.task_running = true;
        thread::spawn(move || {

            let socket: UdpSocket;
            match UdpSocket::bind("0.0.0.0:50505") {
                Ok(sck) => {
                    println!("UDP server listening on 0.0.0.0:50505");
                    let _ = sck.set_read_timeout(Some(Duration::from_millis(100)));
                    socket = sck;
                }
                Err(e) => {
                    println!("cannot start APIServer {}", e);
                    return ;
                }
            }
    
            let mut buf = [0u8; 1024];
            let mut current_played_song = GetCurrentTrackRes {
                song_name: String::new(),
                song_id: String::new(),
                song_artists: Vec::new(),
                song_uri: String::new(),
            };
    
            loop {
                match self.player_events.try_recv() {
                    Ok(data) => {
                        match data {
                            librespot_playback::player::PlayerEvent::TrackChanged { audio_item } => {
                                println!("changing currently played song to {}", audio_item.name);
                                current_played_song.song_name = audio_item.name;
                                current_played_song.song_uri = audio_item.uri;
                                if audio_item.track_id.item_type == SpotifyItemType::Track {
                                    current_played_song.song_id = audio_item.track_id.id.to_string();
                                }
                                match audio_item.unique_fields {
                                    librespot_metadata::audio::UniqueFields::Track { artists, album: _, album_artists: _, popularity: _, number: _, disc_number: _ } => {
                                        current_played_song.song_artists = artists.iter().map(|artist| artist.name.clone()).collect();
                                    }
                                    librespot_metadata::audio::UniqueFields::Episode { description: _, publish_time: _, show_name: _ } => {
                                        current_played_song.song_artists = Vec::new();
                                    }
                                }
                            },
                            librespot_playback::player::PlayerEvent::PlayRequestIdChanged { play_request_id: _ } => {
                                
                            }
                            librespot_playback::player::PlayerEvent::Stopped { play_request_id: _, track_id: _ } => {

                            }
                            librespot_playback::player::PlayerEvent::Loading { play_request_id: _, track_id: _, position_ms: _ } => {
                            
                            },
                            librespot_playback::player::PlayerEvent::Preloading { track_id: _ } => {

                            },
                            librespot_playback::player::PlayerEvent::Playing { play_request_id: _, track_id: _, position_ms: _ } => {

                            },
                            librespot_playback::player::PlayerEvent::Paused { play_request_id: _, track_id: _, position_ms: _ } => {

                            },
                            librespot_playback::player::PlayerEvent::TimeToPreloadNextTrack { play_request_id: _, track_id: _ } => {

                            },
                            librespot_playback::player::PlayerEvent::EndOfTrack { play_request_id: _, track_id: _ } => {

                            },
                            librespot_playback::player::PlayerEvent::Unavailable { play_request_id: _, track_id: _ } => {

                            },
                            librespot_playback::player::PlayerEvent::VolumeChanged { volume: _ } => {

                            },
                            librespot_playback::player::PlayerEvent::PositionCorrection { play_request_id: _, track_id: _, position_ms: _ } => {
                                
                            },
                            librespot_playback::player::PlayerEvent::Seeked { play_request_id: _, track_id: _, position_ms: _ } => {

                            },
                            librespot_playback::player::PlayerEvent::SessionConnected { connection_id: _, user_name: _ } => {

                            },
                            librespot_playback::player::PlayerEvent::SessionDisconnected { connection_id: _, user_name: _ } => {

                            },
                            librespot_playback::player::PlayerEvent::SessionClientChanged { client_id: _, client_name: _, client_brand_name: _, client_model_name: _ } => {

                            },
                            librespot_playback::player::PlayerEvent::ShuffleChanged { shuffle: _ } => {

                            },
                            librespot_playback::player::PlayerEvent::RepeatChanged { repeat: _ } => {

                            },
                            librespot_playback::player::PlayerEvent::AutoPlayChanged { auto_play: _ } => {

                            },
                            librespot_playback::player::PlayerEvent::FilterExplicitContentChanged { filter: _ } => {

                            },
                        }
                    }
                    Err(_e) => {

                    }
                }

                match self.cmd_rx.try_recv() {
                    Ok(data) => {
                        match data {
                            ApiServerCommand::SetSpirc(spirc) => {
                                println!("setting spirc");
                                self.spirc = Some(spirc);
                            }
                            ApiServerCommand::Quit => break,
                        }
                    }
    
                    Err(_e) => {
                    }
                }
    
                match socket.recv_from(&mut buf) {
                    Ok(data) => {
    
                        let message = String::from_utf8_lossy(&buf[..data.0]);
                        println!("Received '{}' from {}", message, data.1.ip().to_string());
        
                        match message.trim() {
                            "next" => {
                                if let Some(spirc) = &self.spirc {
                                    println!("calling next song {}", data.1.ip().to_string());
                                    let _ = spirc.next();
                                }
                            }

                            "pause" => {
                                if let Some(spirc) = &self.spirc {
                                    println!("calling pause");
                                    let _ = spirc.pause();
                                }
                            }

                            "current_track" => {
                                match serde_json::to_string(&current_played_song) {
                                    Ok(json) => {
                                        match socket.send_to(json.as_bytes(), data.1) {
                                            Ok(_) => {},
                                            Err(e) => {
                                                println!("cannot send data to {}, {}", data.1.ip().to_string(), e);
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        println!("cannot parse data to json, {}", e);
                                    },
                                }
                            }

                            "resume" => {
                                if let Some(spirc) = &self.spirc {
                                    println!("calling resume");
                                    let _ = spirc.activate();
                                    let _ = spirc.play();
                                }
                            }

                            _ => {
                                println!("Unknown command: {}", message.trim());
                            }
                        }
                    
                    }
                    Err(_e) => {
                    }
                }
            }
            debug!("Shutting down ApiServerTask...");
            self.task_running = false;
        }
    );
    }
}