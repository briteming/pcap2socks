use std::collections::HashMap;
use std::fmt::{self, Display, Formatter};
use std::io::{self, Write};
use std::net::{Ipv4Addr, SocketAddrV4};
use std::sync::{Arc, Mutex};

pub mod args;
pub mod packet;
pub mod pcap;
pub mod socks;
use crate::socks::SocksDatagram;
use log::{debug, trace, warn, Level, LevelFilter};
use packet::layer::arp::Arp;
use packet::layer::ethernet::Ethernet;
use packet::layer::ipv4::Ipv4;
use packet::layer::tcp::Tcp;
use packet::layer::udp::Udp;
use packet::layer::{Layer, LayerType, LayerTypes, Layers};
use packet::Indicator;
use pcap::{Interface, Receiver, Sender};

/// Sets the logger.
pub fn set_logger(flags: &args::Flags) {
    use env_logger::fmt::Color;

    let level = match &flags.vverbose {
        true => LevelFilter::Trace,
        false => match flags.verbose {
            true => LevelFilter::Debug,
            false => LevelFilter::Info,
        },
    };
    env_logger::builder()
        .filter_level(level)
        .format(|buf, record| {
            let mut style = buf.style();

            let level = match &record.level() {
                Level::Error => style.set_bold(true).set_color(Color::Red).value("error: "),
                Level::Warn => style
                    .set_bold(true)
                    .set_color(Color::Yellow)
                    .value("warning: "),
                Level::Info => style.set_bold(true).set_color(Color::Green).value(""),
                _ => style.set_color(Color::Rgb(165, 165, 165)).value(""),
            };
            writeln!(buf, "{}{}", level, record.args())
        })
        .init();
}

/// Gets a list of available network interfaces for the current machine.
pub fn interfaces() -> Vec<Interface> {
    pcap::interfaces()
        .into_iter()
        .filter(|inter| !inter.is_loopback)
        .collect()
}

/// Gets an available network iterface match the name.
pub fn interface(name: Option<String>) -> Option<Interface> {
    let mut inters = interfaces();
    if inters.len() > 1 {
        if let None = name {
            return None;
        }
    }
    if let Some(inter_name) = name {
        inters.retain(|current_inter| current_inter.name == inter_name);
    }
    if inters.len() <= 0 {
        return None;
    }

    Some(inters[0].clone())
}

const INITIAL_PORT: u16 = 32768;

/// Represents the proxy redirects pcap traffic to SOCKS.
pub struct Proxy {
    hardware_addr: pnet::datalink::MacAddr,
    publish: Option<Ipv4Addr>,
    src: Ipv4Addr,
    dst: SocketAddrV4,
    tx: Arc<Mutex<Sender>>,
    ipv4_identification: u16,
    tcp_sequence: u32,
    tcp_acknowledgement: u32,
    next_udp_port: u16,
    datagrams: Vec<Option<SocksDatagram>>,
    datagram_remote_to_local_map: Vec<u16>,
    datagram_local_to_remote_map: Vec<u16>,
}

impl Proxy {
    /// Opens an `Interface` for proxy.
    pub fn open(
        inter: &Interface,
        publish: Option<Ipv4Addr>,
        src: Ipv4Addr,
        dst: SocketAddrV4,
    ) -> io::Result<(Proxy, Receiver)> {
        let (tx, rx) = inter.open()?;

        Ok((
            Proxy {
                hardware_addr: inter.hardware_addr,
                publish,
                src,
                dst,
                tx: Arc::new(Mutex::new(tx)),
                ipv4_identification: 0,
                tcp_sequence: 0,
                tcp_acknowledgement: 0,
                next_udp_port: INITIAL_PORT,
                datagrams: (0..u16::MAX).map(|_| None).collect(),
                datagram_local_to_remote_map: vec![0; u16::MAX as usize],
                datagram_remote_to_local_map: vec![0; u16::MAX as usize],
            },
            rx,
        ))
    }

    /// Get the sender of the `Proxy`.
    fn get_tx(&self) -> Arc<Mutex<Sender>> {
        self.tx.clone()
    }

    // Handles the proxy.
    pub fn handle(&mut self, rx: &mut Receiver) -> io::Result<()> {
        loop {
            let frame = match rx.next() {
                Ok(frame) => frame,
                Err(e) => {
                    if e.kind() == io::ErrorKind::TimedOut {
                        continue;
                    }
                    return Err(e);
                }
            };

            if let Some(ref indicator) = Indicator::from(frame) {
                trace!("receive from pcap: {}", indicator);

                if let Some(t) = indicator.get_network_type() {
                    match t {
                        LayerTypes::Arp => {
                            if let Err(ref e) = self.handle_arp(indicator) {
                                warn!("handle {}: {}", t, e);
                            };
                        }
                        LayerTypes::Ipv4 => {
                            if let Err(ref e) = self.handle_ipv4(indicator, frame) {
                                warn!("handle {}: {}", t, e);
                            };
                        }
                        _ => {}
                    };
                };
            };
        }
    }

    fn handle_arp(&self, indicator: &Indicator) -> io::Result<()> {
        if let Some(publish) = self.publish {
            if let Some(arp) = indicator.get_arp() {
                if arp.is_request_of(self.src, publish) {
                    debug!(
                        "receive from pcap: {} ({} Bytes)",
                        indicator.brief(),
                        indicator.get_size()
                    );

                    // Reply
                    let new_arp = Arp::reply(&arp, self.hardware_addr);
                    let new_ethernet = Ethernet::new(
                        new_arp.get_type(),
                        new_arp.get_src_hardware_addr(),
                        new_arp.get_dst_hardware_addr(),
                    )
                    .unwrap();
                    let new_indicator = Indicator::new(
                        Layers::Ethernet(new_ethernet),
                        Some(Layers::Arp(new_arp)),
                        None,
                    );
                    trace!("send to pcap {}", new_indicator);

                    // Serialize
                    let size = new_indicator.get_size();
                    let mut buffer = vec![0u8; size];
                    new_indicator.serialize(&mut buffer)?;

                    // Send
                    self.get_tx()
                        .lock()
                        .unwrap()
                        .send_to(&buffer, None)
                        .unwrap_or(Ok(()))?;
                    debug!("send to pcap: {} ({} Bytes)", new_indicator.brief(), size);
                }
            };
        };

        Ok(())
    }

    fn handle_ipv4(&mut self, indicator: &Indicator, buffer: &[u8]) -> io::Result<()> {
        if let Some(ref ipv4) = indicator.get_ipv4() {
            if ipv4.get_src() == self.src {
                debug!(
                    "receive from pcap: {} ({} Bytes)",
                    indicator.brief(),
                    buffer.len()
                );

                if ipv4.is_fragment() {
                    // Fragment
                } else {
                    if let Some(t) = indicator.get_transport_type() {
                        match t {
                            LayerTypes::Tcp => {}
                            LayerTypes::Udp => self.handle_udp(indicator, buffer)?,
                            _ => {}
                        };
                    }
                }
            }
        }

        Ok(())
    }

    fn handle_udp(&mut self, indicator: &Indicator, buffer: &[u8]) -> io::Result<()> {
        if let Some(ref udp) = indicator.get_udp() {
            let port = self.get_local_udp_port(udp.get_src());

            // Bind
            if let None = self.datagrams[port as usize] {
                self.datagrams[port as usize] = match SocksDatagram::bind(
                    SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port),
                    self.dst,
                ) {
                    Ok(datagram) => Some(datagram),
                    Err(e) => return Err(e),
                };
            };

            // Send
            self.datagrams[port as usize].as_ref().unwrap().send_to(
                &buffer[indicator.get_size()..],
                SocketAddrV4::new(udp.get_dst_ip_addr(), udp.get_dst()),
            )?;
            debug!(
                "send to SOCKS: {}: {} -> {} ({} Bytes)",
                udp.get_type(),
                SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, port),
                self.dst,
                buffer.len() - indicator.get_size()
            );
        }

        Ok(())
    }

    /// Get the remote UDP port according to the given local UDP port.
    fn get_remote_udp_port(&self, local_port: u16) -> Option<u16> {
        let port = self.datagram_local_to_remote_map[local_port as usize];
        if port == 0 {
            return None;
        }

        Some(port)
    }

    /// Get the local UDP port distributed according to the given remove UDP port or distribute a new one.
    fn get_local_udp_port(&mut self, remote_port: u16) -> u16 {
        if self.datagram_remote_to_local_map[remote_port as usize] == 0 {
            self.datagram_local_to_remote_map[self.next_udp_port as usize] = remote_port;
            self.datagram_remote_to_local_map[remote_port as usize] = self.next_udp_port;

            // To next port
            self.next_udp_port = self.next_udp_port + 1;
            if self.next_udp_port > u16::MAX {
                self.next_udp_port = INITIAL_PORT;
            }
        }

        self.datagram_remote_to_local_map[remote_port as usize]
    }
}
