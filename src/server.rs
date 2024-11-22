use std::{net::UdpSocket, thread, time::Duration};

use librespot_connect::spirc::Spirc;
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
    pub async fn spawn() -> Result<ApiServerHandler, ApiServerError> {
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let task_running = false;

        let api_server_task = ApiServerTask {
            spirc: None,
            cmd_rx,
            task_running
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

        }
        println!("APIServer task stopped")
    }
}

struct ApiServerTask {
    spirc: Option<Spirc>,
    cmd_rx: mpsc::UnboundedReceiver<ApiServerCommand>,
    task_running: bool,
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
    
            loop {
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
    
                    Err(e) => {
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
                    Err(e) => {
                    }
                }
            }
            debug!("Shutting down ApiServerTask...");
            self.task_running = false;
        }
    );
    }
}