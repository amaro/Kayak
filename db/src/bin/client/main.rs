/* Copyright (c) 2018 University of Utah
 *
 * Permission to use, copy, modify, and distribute this software for any
 * purpose with or without fee is hereby granted, provided that the above
 * copyright notice and this permission notice appear in all copies.
 *
 * THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR(S) DISCLAIM ALL WARRANTIES
 * WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
 * MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL AUTHORS BE LIABLE FOR
 * ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
 * WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
 * ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
 * OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.
 */

#![feature(use_extern_macros)]
#![feature(integer_atomics)]

extern crate db;

use std::sync::Arc;
use std::cell::Cell;
use std::fmt::Display;
use std::str::FromStr;
use std::mem::size_of;
use std::net::Ipv4Addr;

use db::e2d2::headers::*;
use db::e2d2::interface::*;
use db::e2d2::scheduler::*;
use db::e2d2::scheduler::NetBricksContext as NetbricksContext;
use db::e2d2::common::EmptyMetadata;
use db::e2d2::config::{NetbricksConfiguration, PortConfiguration};
use db::config;
use db::log::*;
use db::wireformat::{GetRequest, InvokeRequest};

// Type aliases for convenience.
type UdpPacket = Packet<UdpHeader, EmptyMetadata>;
type IpPacket = Packet<IpHeader, EmptyMetadata>;

mod ycsb;

/// This type implements a simple request generator for Sandstorm.
/// When the generate_request() method on this type is called, an RPC request
/// is created and sent out over a network interface.
struct RequestGenerator<T>
where
    T: PacketTx + PacketRx + Display + Clone + 'static,
{
    // The network interface over which requests will be sent out.
    net_port: T,

    // If true, the requests generated will be invoke() RPCs. If false,
    // the requests generated will be regular get() RPCs.
    use_invoke: bool,

    // The UDP header on each packet generated by the request generator.
    req_udp_header: UdpHeader,

    // The IP header on each packet generated by the request generator.
    // Currently using IPv4.
    req_ip_header: IpHeader,

    // The MAC header on each packet generated by the request generator.
    req_mac_header: MacHeader,

    // Tracks number of packets sent to the server for occasional debug messages.
    requests_sent: Cell<u64>,
}

impl<T> RequestGenerator<T>
where
    T: PacketTx + PacketRx + Display + Clone + 'static,
{
    /// This function returns an instance of RequestGenerator. The RPC, UDP, IP,
    /// and MAC headers on packets generated by this instance are pre-computed
    /// in this method.
    fn new(config: &config::ClientConfig, port: T) -> RequestGenerator<T> {
        // Create UDP, IP, and MAC headers that are placed on all outgoing packets.
        // Length fields are tweaked on a request-by-request basis in the outgoing
        // packets.
        let mut udp_header: UdpHeader = UdpHeader::new();
        udp_header.set_src_port(config.udp_port);
        udp_header.set_dst_port(config.server_udp_port);
        udp_header.set_length(8);
        udp_header.set_checksum(0);

        // Create a common ip header.
        let ip_src_addr: u32 =
            u32::from(Ipv4Addr::from_str(&config.ip_address).expect("Failed to create source IP."));
        let ip_dst_addr: u32 = u32::from(
            Ipv4Addr::from_str(&config.server_ip_address)
                .expect("Failed to create destination IP."),
        );

        let mut ip_header: IpHeader = IpHeader::new();
        ip_header.set_src(ip_src_addr);
        ip_header.set_dst(ip_dst_addr);
        ip_header.set_ttl(128);
        ip_header.set_version(4);
        ip_header.set_ihl(5);
        ip_header.set_length(20);

        // Create a common mac header.
        let mut mac_header: MacHeader = MacHeader::new();
        mac_header.src = config.parse_mac();
        mac_header.dst = config.parse_server_mac();
        mac_header.set_etype(0x0800);

        warn!("use_invoke: {}", config.use_invoke);

        RequestGenerator {
            net_port: port.clone(),
            // If true, invoke() RPC requests will be generated. If false,
            // regular get() RPCs will be generated.
            use_invoke: config.use_invoke,
            req_udp_header: udp_header,
            req_ip_header: ip_header,
            req_mac_header: mac_header,
            requests_sent: Cell::new(0),
        }
    }

    /// Allocate a packet and push MAC, IP, and UDP headers on it taken
    /// from the server desination specificated in `new()`. Panics
    /// if allocation or header manipulation fails at any point.
    #[inline]
    fn create_request(&self) -> UdpPacket {
        new_packet().expect("Failed to allocate packet for request!")
            .push_header(&self.req_mac_header)
            .expect("Failed to push MAC header into request!")
            .push_header(&self.req_ip_header)
            .expect("Failed to push IP header into request!")
            .push_header(&self.req_udp_header)
            .expect("Failed to push UDP header into request!")
    }

    /// Compute and populate UDP and IP header length fields for `request`.
    /// This should be called at the tail of every `construct()` call,
    /// otherwise headers may indicate incorrect payload sizes.
    fn fixup_header_length_fields(mut request: UdpPacket) -> IpPacket
    {
        let udp_len = (size_of::<UdpHeader>() + request.get_payload().len()) as u16;
        request.get_mut_header().set_length(udp_len);

        let mut request = request.deparse_header(size_of::<IpHeader>());
        request.get_mut_header().set_length(size_of::<IpHeader>() as u16 + udp_len);

        request
    }

    /// Allocate and populate a packet that requests a server "get" operation.
    /// The returned request packet is implicitly addressed to the server
    /// specified by `new()`.
    /// May panic if there is a problem allocating the packet or constructing
    /// headers.
    ///
    /// # Arguments
    ///  * `tenant`: Id of the tenant requesting the item.
    ///  * `table_id`: Id of the table from which the key is looked up.
    ///  * `key`: Byte string of key whose value is to be fetched. Limit 64 KB.
    /// # Return
    ///  Packet populated with the request parameters.
    #[inline]
    fn create_get_request(&self,
                          tenant: u32,
                          table_id: u64,
                          key: &[u8])
        -> IpPacket
    {
        if key.len() > u16::max_value() as usize {
            // TODO(stutsman) This function should return Result instead of panic.
            panic!("Key too long ({} bytes).", key.len());
        }

        let mut request = self.create_request()
                                .push_header(&GetRequest::new(tenant, table_id, key.len() as u16))
                                .expect("Failed to push RPC header into request!");

        request.add_to_payload_tail(key.len(), &key)
                .expect("Failed to write key into get() request!");

        Self::fixup_header_length_fields(request.deparse_header(size_of::<UdpHeader>()))
    }

    /// Allocate and populate a packet that requests a server "invoke" operation.
    /// The returned request packet is implicitly addressed to the server
    /// specified by `new()`.
    /// May panic if there is a problem allocating the packet or constructing
    /// headers.
    ///
    /// # Arguments
    ///  * `tenant`:   Id of the tenant requesting the item.
    ///  * `name_len`: Length of the extensions name inside the payload.
    ///  * `args_len`: Length of the arguments inside the payload.
    /// # Return
    ///  Packet populated with the request parameters.
    #[inline]
    fn create_invoke_request(&self,
                             tenant: u32,
                             name_len: usize,
                             args_len: usize,
                             payload: &[u8])
        -> IpPacket
    {
        if name_len > u32::max_value() as usize {
            // TODO(stutsman) This function should return Result instead of panic.
            panic!("Name too long ({} bytes).", name_len);
        }

        if args_len > u32::max_value() as usize {
            // TODO(stutsman) This function should return Result instead of panic.
            panic!("Args too long ({} bytes).", args_len);
        }

        let mut request = self.create_request()
                                .push_header(&InvokeRequest::new(tenant, name_len as u32,
                                                                 args_len as u32))
                                .expect("Failed to push RPC header into request!");

        request.add_to_payload_tail(payload.len(), &payload)
                .expect("Failed to write args into invoke() request!");

        Self::fixup_header_length_fields(request.deparse_header(size_of::<UdpHeader>()))
    }

    /// This method generates a simple get() RPC request and sends it
    /// out the network interface.
    #[inline]
    fn generate_request(&self) {
        let request = if self.use_invoke {
                let mut payload: Vec<u8> = Vec::new();
                let table = [1, 0, 0, 0, 0, 0, 0, 0];
                let key: [u8; 30] = [0; 30];
                let name = "get".as_bytes();
                payload.extend_from_slice(name);
                payload.extend_from_slice(&table);
                payload.extend_from_slice(&key);
                self.create_invoke_request(1, name.len(), payload.len() - name.len(), payload.as_slice())
            } else {
                self.create_get_request(1, 1, &[0; 30])
            };

        // Send the request out the network.
        unsafe {
            let mut pkts = [request.get_mbuf()];

            match self.net_port.send(&mut pkts) {
                Ok(sent) => {
                    if sent < pkts.len() as u32 {
                        println!("WARNING: Failed to send all packets!");
                    }
                }

                Err(ref err) => {
                    println!("Error on packet send: {}", err);
                    std::process::exit(1);
                }
            }
        }

        let r = self.requests_sent.get();
        if r & 0xffffff == 0 {
            info!("Sent many requests...");
        }
        self.requests_sent.set(r + 1);

    }
}

// Implementation of the Executable trait for RequestGenerator. This trait
// allows the generator to be scheduled by Netbricks.
impl<T> Executable for RequestGenerator<T>
where
    T: PacketTx + PacketRx + Display + Clone + 'static,
{
    /// When called, this method generates a request.
    ///
    /// Once the generator has been added to Netbricks, the scheduler
    /// constantly invokes this method, effectively resulting in requests
    /// being sent out the network.
    fn execute(&mut self) {
        self.generate_request();
    }

    /// This method returns a vector of tasks that need to be executed by
    /// the scheduler before callin execute() on RequestGenerator.
    ///
    /// \return
    ///     A vector of tasks that RequestGenerator depends on.
    fn dependencies(&mut self) -> Vec<usize> {
        vec![]
    }
}

/// This function adds a request generator (RequestGenerator) to a Netbricks
/// pipeline. This function is passed in as a closure to Netbricks, and gets
/// run once on each Netbricks scheduler during setup.
fn setup_client<T, S>(config: &config::ClientConfig, ports: Vec<T>, scheduler: &mut S)
where
    T: PacketTx + PacketRx + Display + Clone + 'static,
    S: Scheduler + Sized,
{
    if ports.len() != 1 {
        println!("ERROR: Client should be configured with exactly 1 port!");
        std::process::exit(1);
    }

    let client: RequestGenerator<T> = RequestGenerator::new(config, ports[0].clone());

    // Add the request generator to a netbricks pipeline.
    match scheduler.add_task(client) {
        Ok(_) => {
            println!("Successfully added client to a Netbricks pipeline.");
        }

        Err(ref err) => {
            println!("Error while adding to Netbricks pipeline {}", err);
            std::process::exit(1);
        }
    }
}

/// Returns a struct of type NetbricksConfiguration which can be used to
/// initialize Netbricks with a default set of parameters.
///
/// If used to initialize Netbricks, this struct will run the parent client
/// thread on core 0, and one scheduler on core 1. Packet buffers will be
/// allocated from a 2 GB memory pool, with 64 MB cached at core 1. DPDK will
/// be initialized as a primary process without any additional arguments. A
/// single network interface/port with 1 transmit queue, 1 receive queue, 256
/// receive descriptors, and 256 transmit descriptors will be made available to
/// Netbricks. Loopback, hardware transmit segementation offload, and hardware
/// checksum offload will be disabled on this port.
fn get_default_netbricks_config() -> NetbricksConfiguration {
    // General arguments supplied to netbricks.
    let net_config_name = String::from("client");
    let dpdk_secondary: bool = false;
    let net_primary_core: i32 = 0;
    let net_cores: Vec<i32> = vec![1];
    let net_strict_cores: bool = true;
    let net_pool_size: u32 = 2048 - 1;
    let net_cache_size: u32 = 64;
    let net_dpdk_args: Option<String> = None;

    // Port configuration. Required to configure the physical network interface.
    let net_port_name = String::from("0000:04:00.1");
    let net_port_rx_queues: Vec<i32> = net_cores.clone();
    let net_port_tx_queues: Vec<i32> = net_cores.clone();
    let net_port_rxd: i32 = 256;
    let net_port_txd: i32 = 256;
    let net_port_loopback: bool = false;
    let net_port_tcp_tso: bool = false;
    let net_port_csum_offload: bool = false;

    let net_port_config = PortConfiguration {
        name: net_port_name,
        rx_queues: net_port_rx_queues,
        tx_queues: net_port_tx_queues,
        rxd: net_port_rxd,
        txd: net_port_txd,
        loopback: net_port_loopback,
        tso: net_port_tcp_tso,
        csum: net_port_csum_offload,
    };

    // The set of ports used by netbricks.
    let net_ports: Vec<PortConfiguration> = vec![net_port_config];

    NetbricksConfiguration {
        name: net_config_name,
        secondary: dpdk_secondary,
        primary_core: net_primary_core,
        cores: net_cores,
        strict: net_strict_cores,
        ports: net_ports,
        pool_size: net_pool_size,
        cache_size: net_cache_size,
        dpdk_args: net_dpdk_args,
    }
}

/// This function configures and initializes Netbricks. In the case of a
/// failure, it causes the program to exit.
///
/// Returns a Netbricks context which can be used to setup and start the
/// server/client.
fn config_and_init_netbricks() -> NetbricksContext {
    let net_config: NetbricksConfiguration = get_default_netbricks_config();

    // Initialize Netbricks and return a handle.
    match initialize_system(&net_config) {
        Ok(net_context) => {
            return net_context;
        }

        Err(ref err) => {
            println!("Error during Netbricks init: {}", err);
            // TODO: Drop NetbricksConfiguration?
            std::process::exit(1);
        }
    }
}

fn main() {
    db::env_logger::init().expect("ERROR: failed to initialize logger!");

    let config = config::ClientConfig::load();
    info!("Starting up Sandstorm client with config {:?}", config);

    // Setup Netbricks.
    let mut net_context: NetbricksContext = config_and_init_netbricks();

    // Setup the client pipeline.
    net_context.start_schedulers();
    net_context.add_pipeline_to_run(Arc::new(
        move |ports, scheduler: &mut StandaloneScheduler| setup_client(&config, ports, scheduler),
    ));

    // Run the client.
    net_context.execute();

    loop {}

    // Stop the client.
    // net_context.stop();
}