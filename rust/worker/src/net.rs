// Copyright (c) Facebook, Inc. and its affiliates.
use bytes::Bytes;
use dag_core::messages::WorkerChannelType;
use dag_core::types::{Transaction, WorkerMessage, WorkerMessageCommand};
use futures::select;
use futures::sink::SinkExt;
use futures::stream::FuturesOrdered;
use futures::stream::StreamExt;
use futures::FutureExt;
use log::*;
use std::error;
use std::net::SocketAddr;
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::mpsc::Sender;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

#[cfg(test)]
#[path = "tests/net_tests.rs"]
mod net_tests;

pub async fn worker_server_start(
    url: String,
    worker_message_output: Sender<WorkerMessageCommand>,
    synchronize_message_output: Sender<WorkerMessageCommand>,
    transaction_output: Sender<(SocketAddr, Transaction)>,
) -> Result<(), Box<dyn error::Error>> {
    let listener = TcpListener::bind(url).await?;

    loop {
        // Listen for new connections.
        let (socket, _) = listener.accept().await?;
        let worker_out = worker_message_output.clone();
        let sync_out = synchronize_message_output.clone();
        let transact_out = transaction_output.clone();

        tokio::spawn(async move {
            let ip = socket.peer_addr().unwrap(); // TODO: check error here.
            let mut transport = Framed::new(socket, LengthDelimitedCodec::new());

            // TODO: Do some authentication here
            if let Some(Ok(channel_type_data)) = transport.next().await {
                let channel_type: WorkerChannelType =
                    match bincode::deserialize(&channel_type_data[..]) {
                        Err(e) => {
                            warn!("Cannot parse banner; err = {:?}", e);
                            return;
                        }
                        Ok(channel_type) => channel_type,
                    };

                let ok = Bytes::from("OK");
                if let Err(e) = transport.send(ok).await {
                    warn!("failed to write to socket; err = {:?}", e);
                    return;
                }

                match channel_type {
                    WorkerChannelType::Worker => {
                        debug!("handling worker messages");
                        handle_worker_channel(transport, worker_out, sync_out).await;
                    }
                    WorkerChannelType::Transaction => {
                        debug!("handling transactions");
                        handle_transaction_channel(transport, transact_out, ip).await;
                    }
                }
            } else {
                warn!("Channel is broken on channel type.");
            }
        });
    }
}

pub async fn set_channel_type(
    transport: &mut Framed<TcpStream, LengthDelimitedCodec>,
    label: WorkerChannelType,
) {
    let header = Bytes::from(bincode::serialize(&label).expect("Bad serialization"));
    transport.send(header).await.expect("Error sending");
    let _n = transport.next().await.expect("Error on test receive");
}

async fn handle_worker_channel(
    mut transport: Framed<TcpStream, LengthDelimitedCodec>,
    mut worker_out: Sender<WorkerMessageCommand>,
    mut sync_out: Sender<WorkerMessageCommand>,
) {
    let ok = Bytes::from("OK");
    let notfound = Bytes::from("NOTFOUND");
    let mut responses_ordered = FuturesOrdered::new();

    // In a loop, read data from the socket and write the data back.
    loop {
        select! {
            worker_message_data = transport.next().fuse() => {

                if worker_message_data.is_none() {
                    // Channel is closed, nothing to do any more.
                    return;
                }
                let worker_message_data = worker_message_data.unwrap();

                match worker_message_data {
                    Ok(data) => {
                        // Send the transaction on the channel.
                        // Decode the data.
                        let msg: WorkerMessage = match bincode::deserialize(&data[..]) {
                            Err(e) => {
                                warn!("parsing Error, closing channel; err = {:?}", e);
                                return;
                            }
                            Ok(msg) => msg,
                        };

                        // Determine what we send back on None.
                        let (must_wait, on_none, output_channel) = match &msg {
                            WorkerMessage::Query(..) => (true, &notfound, &mut worker_out),
                            /*
                                For Batch and Sync WorkerMessages we schedule
                                the command for processing and we respond immediately.
                            */
                            WorkerMessage::Synchronize(..) => (false, &ok, &mut sync_out),
                            _ => (false, &ok, &mut worker_out),
                        };

                        let (cmd, resp) = WorkerMessageCommand::new(msg);

                        if let Err(e) = (*output_channel).send(cmd).await {
                            error!("channel has closed; err = {:?}", e);
                            return;
                        }

                        responses_ordered.push(async move {
                            if must_wait {
                                match resp.get().await {
                                    None => {
                                        on_none.clone()
                                    },
                                    Some(response_message) => {
                                        let data = bincode::serialize(&response_message).unwrap();
                                        Bytes::from(data)
                                    }
                                }
                            } else {
                                on_none.clone()
                            }
                        });
                    },
                    Err(_e) => {
                        return;
                    }
                }
            },
            response = responses_ordered.select_next_some() => {
                if let Err(e) = transport.send(response).await {
                    error!("failed to write to socket; err = {:?}", e);
                    return;
                }
            }

        }
    }
}

async fn handle_transaction_channel(
    mut transport: Framed<TcpStream, LengthDelimitedCodec>,
    transaction_out: Sender<(SocketAddr, Transaction)>,
    ip: SocketAddr,
) {
    let ok_response = Bytes::from("OK");

    // In a loop, read data from the socket and write the data back.
    while let Some(transaction_data) = transport.next().await {
        match transaction_data {
            Ok(data) => {
                // Send the transaction on the channel.
                let output = (ip, data.to_vec());
                if let Err(e) = transaction_out.send(output).await {
                    error!("channel has closed; err = {:?}", e);
                    return;
                }

                // Write the data back.
                if let Err(e) = transport.send(ok_response.clone()).await {
                    error!("failed to write to socket; err = {:?}", e);
                    return;
                }
            }
            Err(e) => {
                error!("Socket data ended; err = {:?}", e);
                return;
            }
        }
    }
}
