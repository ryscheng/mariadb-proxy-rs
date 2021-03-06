use byteorder::{BigEndian, ByteOrder};
use futures::{
    channel::mpsc::{Receiver, Sender},
    lock::Mutex,
    select,
    sink::SinkExt,
    FutureExt, StreamExt,
};
//use futures_util::{
//    future::FutureExt,
//    stream::StreamExt,
//};
use std::{
    io::{Error, ErrorKind},
    sync::Arc,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt, Result};

use crate::{
    packet::{DatabaseType, Packet, PacketType, POSTGRES_IDS},
    packet_handler::{Direction, PacketHandler},
};

pub struct Pipe<T: AsyncReadExt, U: AsyncWriteExt> {
    name: String,
    db_type: DatabaseType,
    packet_handler: Arc<Mutex<dyn PacketHandler + Send>>,
    direction: Direction,
    source: T,
    sink: U,
}

impl<T: AsyncReadExt + Unpin, U: AsyncWriteExt + Unpin> Pipe<T, U> {
    pub fn new(
        name: String,
        db_type: DatabaseType,
        packet_handler: Arc<Mutex<dyn PacketHandler + Send>>,
        direction: Direction,
        reader: T,
        writer: U,
    ) -> Pipe<T, U> {
        Pipe {
            name,
            db_type,
            packet_handler,
            direction,
            source: reader,
            sink: writer,
        }
    }

    pub async fn run(
        &mut self,
        mut other_pipe_sender: Sender<Packet>,
        other_pipe_receiver: Receiver<Packet>,
    ) -> Result<()> {
        trace!("[{}]: Running {:?} pipe loop...", self.name, self.direction);
        //let source = Arc::get_mut(&mut self.source).unwrap();
        //let sink = Arc::get_mut(&mut self.sink).unwrap();
        let mut other_pipe_receiver = other_pipe_receiver.into_future().fuse();
        let mut read_buf: Vec<u8> = vec![0_u8; 4096];
        let mut packet_buf: Vec<u8> = Vec::with_capacity(4096);
        let mut write_buf: Vec<u8> = Vec::with_capacity(4096);

        loop {
            select! {
                // Read from the source to read_buf, append to packet_buf
                read_result = self.source.read(&mut read_buf[..]).fuse() => {
                    //let n = self.source.read(&mut read_buf[..]).await?;
                    self.process_read_buf(read_result, &read_buf, &mut packet_buf, &mut write_buf, &mut other_pipe_sender).await?;
                },
                // Support short-circuit
                (packet, recv) = other_pipe_receiver => {
                    self.process_short_circuit(packet, &mut write_buf)?;
                    other_pipe_receiver = recv.into_future().fuse();
                },
            } // end select!

            // Write all to sink
            while !write_buf.is_empty() {
                let n = self.sink.write(&write_buf[..]).await?;
                let _: Vec<u8> = write_buf.drain(0..n).collect();
                self.trace(format!("{} bytes written to sink", n));
            }
        } // end loop
    } // end fn run

    async fn process_read_buf(
        &self,
        read_result: Result<usize>,
        read_buf: &[u8],
        mut packet_buf: &mut Vec<u8>,
        write_buf: &mut Vec<u8>,
        other_pipe_sender: &mut Sender<Packet>,
    ) -> Result<()> {
        if let Ok(n) = read_result {
            if n == 0 {
                let e = self.create_error(format!("Read {} bytes, closing pipe.", n));
                warn!("{}", e.to_string());
                return Err(e);
            }
            packet_buf.extend_from_slice(&read_buf[0..n]);
            self.trace(format!(
                "{} bytes read from source, {} bytes in packet_buf",
                n,
                packet_buf.len()
            ));

            // Process all packets in packet_buf, put into write_buf
            while let Some(packet) = get_packet(self.db_type, &mut packet_buf) {
                self.trace("Processing packet".to_string());
                // TODO: support SSL. For now, respond that we don't support SSL
                // https://www.postgresql.org/docs/12/protocol-flow.html#id-1.10.5.7.11
                if let Ok(PacketType::SSLRequest) = packet.get_packet_type() {
                    self.debug("Got SSLRequest, responding no thanks".to_string());
                    if let Err(_e) = other_pipe_sender
                        .send(Packet::new(self.db_type, String::from("N").into_bytes()))
                        .await
                    {
                        return Err(
                            self.create_error("Error sending SSL response of no".to_string())
                        );
                    }
                } else {
                    let transformed_packet: Packet;
                    {
                        // Scope for self.packet_handler Mutex
                        let mut h = self.packet_handler.lock().await;
                        transformed_packet = match self.direction {
                            Direction::Forward => h.handle_request(&packet).await,
                            Direction::Backward => h.handle_response(&packet).await,
                        };
                    }
                    write_buf.extend_from_slice(&transformed_packet.bytes);
                }
            } // end while
            Ok(())
        } else if let Err(e) = read_result {
            warn!(
                "[{}:{:?}]: Error reading from source",
                self.name, self.direction
            );
            Err(e)
        } else {
            Err(Error::new(ErrorKind::Other, "This should never happen"))
        }
    }

    fn process_short_circuit(&self, packet: Option<Packet>, write_buf: &mut Vec<u8>) -> Result<()> {
        if let Some(p) = packet {
            self.trace(format!(
                "Got short circuit packet of {} bytes",
                p.get_size()
            ));
            write_buf.extend_from_slice(&p.bytes);
            Ok(())
        } else {
            let e = self.create_error("other_pipe_receiver prematurely closed".to_string());
            warn!("{}", e.to_string());
            Err(e)
        }
    }

    fn debug(&self, string: String) {
        debug!("[{}:{:?}]: {}", self.name, self.direction, string);
    }

    fn trace(&self, string: String) {
        trace!("[{}:{:?}]: {}", self.name, self.direction, string);
    }

    fn create_error(&self, string: String) -> Error {
        Error::new(
            ErrorKind::Other,
            format!("[{}:{:?}]: {}", self.name, self.direction, string),
        )
    }
} // end impl

fn get_packet(db_type: DatabaseType, packet_buf: &mut Vec<u8>) -> Option<Packet> {
    match db_type {
        DatabaseType::MariaDB => {
            // Check for header
            if packet_buf.len() < 4 {
                return None;
            }
            let l: usize = (((packet_buf[2] as u32) << 16)
                | ((packet_buf[1] as u32) << 8)
                | packet_buf[0] as u32) as usize;
            let s = 4 + l;
            // Check for entire packet size
            if packet_buf.len() < s {
                return None;
            }
            Some(Packet::new(
                DatabaseType::MariaDB,
                packet_buf.drain(0..s).collect(),
            ))
        } // end MariaDB
        DatabaseType::PostgresSQL => {
            // Nothing in packet_buf
            if packet_buf.is_empty() {
                trace!(
                    "get_packet(PostgresSQL): FAIL packet_buf(size={}) trying to read first byte",
                    packet_buf.len()
                );
                return None;
            }
            let id = packet_buf[0] as char;
            let mut size = 0;
            if POSTGRES_IDS.contains(&id) {
                size += 1;
            }

            // Check if I can read the length field
            if packet_buf.len() < (size + 4) {
                trace!(
                    "get_packet(PostgresSQL): FAIL packet_buf(size={}) trying to read length, firstbyte={:#04x}={}, size={}",
                    packet_buf.len(), packet_buf[0], id, size+4
                );
                return None;
            }
            let length = BigEndian::read_u32(&packet_buf[size..(size + 4)]) as usize; // read length
            size += length;

            // Check if don't have entire packet
            if packet_buf.len() < size {
                trace!(
                    "get_packet(PostgresSQL): FAIL packet_buf(size={}) too small, firstbyte={:#04x}={}, size={}, length={}",
                    packet_buf.len(), packet_buf[0], id, size, length
                );
                return None;
            }
            trace!(
                "get_packet(PostgresSQL): SUCCESS firstbyte={:#04x}={}, size={}, length={}",
                packet_buf[0],
                id,
                size,
                length
            );

            Some(Packet::new(
                DatabaseType::PostgresSQL,
                packet_buf.drain(0..size).collect(),
            ))
        } // end PostgresSQL
    } // end match
} // end get_packet
