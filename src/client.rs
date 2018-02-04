use core::option::Option;
use std::net::SocketAddrV4;
use std::thread::{ spawn, JoinHandle };

use chan::{ Sender, Receiver, async };
use time::{ Duration, Timespec, get_time };
use timer::Timer;
use uuid::Uuid;

use internal::command::Cmd;
use internal::connection::Connection;
use internal::endpoint::Endpoint;
use internal::messaging::Msg;
use internal::messages;
use internal::package::Pkg;
use internal::registry::{ Registry, Outcome };
use internal::types::Settings;

use protobuf;

#[derive(Copy, Clone)]
enum HeartbeatStatus {
    Init,
    Delay(u32, Timespec),
    Timeout(u32, Timespec),
}

enum Heartbeat {
    Valid,
    Failure,
}

struct HealthTracker {
    pkg_num: u32,
    state: HeartbeatStatus,
    heartbeat_delay: Duration,
    heartbeat_timeout: Duration,
}

impl HealthTracker {
    fn new(setts: &Settings) -> HealthTracker {
        HealthTracker {
            pkg_num: 0,
            state: HeartbeatStatus::Init,
            heartbeat_delay: setts.heartbeat_delay,
            heartbeat_timeout: setts.heartbeat_timeout,
        }
    }

    fn incr_pkg_num(&mut self) {
        self.pkg_num += 1;
    }

    fn manage_heartbeat(&mut self, conn: &Connection) -> Heartbeat {
        match self.state {
            HeartbeatStatus::Init => {
                self.state = HeartbeatStatus::Delay(self.pkg_num, get_time());

                Heartbeat::Valid
            },

            HeartbeatStatus::Delay(num, start) => {
                let now = get_time();

                if self.pkg_num != num {
                    self.state = HeartbeatStatus::Delay(self.pkg_num, now);
                } else {
                    if now - start >= self.heartbeat_delay {
                        self.state = HeartbeatStatus::Timeout(self.pkg_num, now);
                        conn.enqueue(Pkg::heartbeat_request());
                    }
                }

                Heartbeat::Valid
            },

            HeartbeatStatus::Timeout(num, start) => {
                let now = get_time();

                if self.pkg_num != num {
                    self.state = HeartbeatStatus::Delay(self.pkg_num, now);

                    Heartbeat::Valid
                } else {
                    if now - start >= self.heartbeat_timeout {
                        Heartbeat::Failure
                    } else {
                        Heartbeat::Valid
                    }
                }
            },
        }
    }
}

enum ConnectionState {
    Init,
    Connecting,
    Connected(Connection),
    Closed,
}

enum ConnectionPhase {
    Reconnecting,
    EndpointDiscovery,
    Establishing,
    Authentication { correlation: Uuid, conn_id: Uuid, started: Timespec },
    Identification { correlation: Uuid, conn_id: Uuid, started: Timespec },
}

struct Attempt {
    started: Timespec,
    tries: u32,
}

impl Attempt {
    fn new() -> Attempt {
        Attempt {
            started: get_time(),
            tries: 0,
        }
    }
}

struct StaticDiscovery {
    addr: SocketAddrV4,
}

impl Discovery for StaticDiscovery {
    fn discover(&mut self, _: Option<&Endpoint>) -> Endpoint {
        Endpoint {
            addr: self.addr,
        }
    }
}

impl StaticDiscovery {
    fn new(addr: SocketAddrV4) -> StaticDiscovery {
        StaticDiscovery {
            addr: addr,
        }
    }
}

trait Discovery {
    fn discover(&mut self, last: Option<&Endpoint>) -> Endpoint;
}

enum Report {
    Continue,
    Quit,
}

struct Driver {
    registry: Registry,
    candidate: Option<Connection>,
    tracker: HealthTracker,
    attempt: Option<Attempt>,
    state: ConnectionState,
    phase: ConnectionPhase,
    last_endpoint: Option<Endpoint>,
    discovery: Box<Discovery>,
}

impl Driver {
    fn new(setts: &Settings, disc: Box<Discovery>) -> Driver {
        Driver {
            registry: Registry::new(setts),
            candidate: None,
            tracker: HealthTracker::new(setts),
            attempt: None,
            state: ConnectionState::Init,
            phase: ConnectionPhase::Reconnecting,
            last_endpoint: None,
            discovery: disc,
        }
    }

    fn discover(&mut self, sender: Sender<Msg>) {
        let endpoint = self.discovery.discover(None);

        self.state = ConnectionState::Connecting;
        self.phase = ConnectionPhase::EndpointDiscovery;

        // TODO - Will be performed in a different thread.
        sender.send(Msg::Establish(endpoint));
    }

    fn on_establish(&mut self, sender: Sender<Msg>, endpoint: Endpoint) {
        self.phase         = ConnectionPhase::Establishing;
        self.candidate     = Some(Connection::new(sender, endpoint.addr));
        self.last_endpoint = Some(endpoint);
    }

    fn on_established(&mut self, id: Uuid) {
        if let Some(conn) = self.candidate.take() {
            if conn.id == id {
                self.state = ConnectionState::Connected(conn);
            }
        }
    }

    fn on_package_arrived(&mut self, pkg: Pkg) {
         self.tracker.incr_pkg_num();

         if let ConnectionState::Connected(ref conn) = self.state {
             match pkg.cmd {
                 Cmd::HeartbeatRequest => {
                     println!("Heartbeat request received");

                     let mut resp = pkg.copy_headers_only();

                     resp.cmd = Cmd::HeartbeatResponse;

                     conn.enqueue(resp);
                 },

                 _ => self.registry.handle(pkg),
             }
         }
    }

    fn on_tick(&mut self) -> Report {
        if let ConnectionState::Connected(ref conn) = self.state {
            if let Heartbeat::Valid = self.tracker.manage_heartbeat(conn) {
                self.registry.check_and_retry(conn);

                Report::Continue
            } else {
                println!("Heartbeat TIMEOUT");

                Report::Quit
            }
        } else {
            Report::Continue
        }
    }
}

fn worker_thread(settings: &Settings, disc: Box<Discovery>, sender: Sender<Msg>, receiver: Receiver<Msg>) {
    let mut driver = Driver::new(settings, disc);
    let     timer  = Timer::new();

    loop {
        if let Some(msg) = receiver.recv() {
            match msg {
                Msg::Start => {
                    let tx1 = sender.clone();
                    let tx2 = sender.clone();

                    timer.schedule_repeating(Duration::milliseconds(200), move || {
                        tx1.send(Msg::Tick);
                    });

                    driver.discover(tx2);
                },

                Msg::Shutdown => {
                    println!("Shutting down...");
                    break;
                },

                Msg::Establish(endpoint) =>
                    driver.on_establish(sender.clone(), endpoint),

                Msg::Established(id) => {
                    driver.on_established(id);
                }

                Msg::Arrived(pkg) => {
                    driver.on_package_arrived(pkg);
                },

                Msg::Tick => {
                    if let Report::Quit = driver.on_tick() {
                        break
                    }
                },
            }
        } else {
            println!("Main bus closed");
            break;
        }
    }
}

pub struct Client {
    worker: JoinHandle<()>,
    sender: Sender<Msg>,
}

impl Client {
    pub fn new(settings: Settings, addr: SocketAddrV4) -> Client {
        let (sender, recv) = async();
        let disc           = Box::new(StaticDiscovery::new(addr));

        let tx     = sender.clone();
        let handle = spawn(move || worker_thread(&settings, disc, sender, recv));

        Client {
            worker: handle,
            sender: tx,
        }
    }

    pub fn start(&self) {
        self.sender.send(Msg::Start);
    }

    pub fn shutdown(&self) {
        self.sender.send(Msg::Shutdown);
    }

    pub fn wait_till_closed(self) {
        self.worker.join().unwrap();
    }
}
