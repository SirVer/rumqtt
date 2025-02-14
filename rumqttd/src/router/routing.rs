use crate::protocol::{
    ConnAck, ConnectReturnCode, Packet, PingResp, PubAck, PubAckReason, PubComp, PubCompReason,
    PubRec, PubRecReason, PubRel, PubRelReason, Publish, QoS, SubAck, SubscribeReasonCode,
    UnsubAck,
};
use crate::router::graveyard::SavedState;
use crate::router::scheduler::{PauseReason, Tracker};
use crate::router::Forward;
use crate::segments::Position;
use crate::*;
use flume::{bounded, Receiver, RecvError, Sender, TryRecvError};
use log::*;
use slab::Slab;
use std::collections::{HashMap, HashSet, VecDeque};
use std::str::Utf8Error;
use std::thread;
use std::time::SystemTime;
use thiserror::Error;

use super::graveyard::Graveyard;
use super::iobufs::{Incoming, Outgoing};
use super::logs::{AckLog, DataLog};
use super::scheduler::{ScheduleReason, Scheduler};
use super::{
    packetid, Connection, DataRequest, Event, FilterIdx, MetricsReply, MetricsRequest,
    Notification, RouterMetrics, ShadowRequest, MAX_CHANNEL_CAPACITY, MAX_SCHEDULE_ITERATIONS,
};

#[derive(Error, Debug)]
pub enum RouterError {
    #[error("Receive error = {0}")]
    Recv(#[from] RecvError),
    #[error("Try Receive error = {0}")]
    TryRecv(#[from] TryRecvError),
    #[error("Disconnection")]
    Disconnected,
    #[error("Topic not utf-8")]
    NonUtf8Topic(#[from] Utf8Error),
    #[error("Bad Tenant")]
    BadTenant(String, String),
    #[error("No matching filters to topic {0}")]
    NoMatchingFilters(String),
    #[error("Unsupported QoS {0:?}")]
    UnsupportedQoS(QoS),
    #[error("Invalid filter prefix {0}")]
    InvalidFilterPrefix(Filter),
}

pub struct Router {
    id: RouterId,
    /// Id of this router. Used to index native commitlog to store data from
    /// local connections
    config: RouterConfig,
    /// Saved state of dead persistent connections
    graveyard: Graveyard,
    /// List of connections
    connections: Slab<Connection>,
    /// Connection map from device id to connection id
    connection_map: HashMap<String, ConnectionId>,
    /// Subscription map to interested connection ids
    subscription_map: HashMap<Filter, HashSet<ConnectionId>>,
    /// Incoming data grouped by connection
    ibufs: Slab<Incoming>,
    /// Outgoing data grouped by connection
    obufs: Slab<Outgoing>,
    /// Data log of all the subscriptions
    datalog: DataLog,
    /// Acks log per connection
    ackslog: Slab<AckLog>,
    /// Scheduler to schedule connections
    scheduler: Scheduler,
    /// Parked requests that are ready because of new data on the subscription
    notifications: VecDeque<(ConnectionId, DataRequest)>,
    /// Channel receiver to receive data from all the active connections and
    /// replicators. Each connection will have a tx handle which they use
    /// to send data and requests to router
    router_rx: Receiver<(ConnectionId, Event)>,
    /// Channel sender to send data to this router. This is given to active
    /// network connections, local connections and replicators to communicate
    /// with this router
    router_tx: Sender<(ConnectionId, Event)>,
    /// Router metrics
    router_metrics: RouterMetrics,
    /// Buffer for cache exchange of incoming packets
    cache: Option<VecDeque<Packet>>,
}

impl Router {
    pub fn new(router_id: RouterId, config: RouterConfig) -> Router {
        let (router_tx, router_rx) = bounded(1000);

        let connections = Slab::with_capacity(config.max_connections);
        let ibufs = Slab::with_capacity(config.max_connections);
        let obufs = Slab::with_capacity(config.max_connections);
        let ackslog = Slab::with_capacity(config.max_connections);

        let router_metrics = RouterMetrics {
            router_id,
            ..RouterMetrics::default()
        };

        let max_connections = config.max_connections;
        Router {
            id: router_id,
            config: config.clone(),
            graveyard: Graveyard::new(),
            connections,
            connection_map: Default::default(),
            subscription_map: Default::default(),
            ibufs,
            obufs,
            datalog: DataLog::new(config).unwrap(),
            ackslog,
            scheduler: Scheduler::with_capacity(max_connections),
            notifications: VecDeque::with_capacity(1024),
            router_rx,
            router_tx,
            router_metrics,
            cache: Some(VecDeque::with_capacity(MAX_CHANNEL_CAPACITY)),
        }
    }

    /// Gets handle to the router. This is not a public method to ensure that link
    /// is created only after the router starts
    fn link(&self) -> Sender<(ConnectionId, Event)> {
        self.router_tx.clone()
    }

    // pub(crate) fn get_replica_handle(&mut self, _replica_id: NodeId) -> (LinkTx, LinkRx) {
    //     unimplemented!()
    // }

    /// Starts the router in a background thread and returns link to it. Link
    /// to communicate with router should only be returned only after it starts.
    /// For that reason, all the public methods should start the router in the
    /// background
    pub fn spawn(mut self) -> Sender<(ConnectionId, Event)> {
        let router = thread::Builder::new().name(format!("router-{}", self.id));
        let link = self.link();
        router
            .spawn(move || {
                let e = self.run(0);
                error!("Router done! Reason = {:?}", e);
            })
            .unwrap();
        link
    }

    /// Waits on incoming events when ready queue is empty.
    /// After pulling 1 event, tries to pull 500 more events
    /// before polling ready queue 100 times (connections)
    fn run(&mut self, count: usize) -> Result<(), RouterError> {
        match count {
            0 => loop {
                self.run_inner()?;
            },
            n => {
                for _ in 0..n {
                    self.run_inner()?;
                }
            }
        };

        Ok(())
    }

    fn run_inner(&mut self) -> Result<(), RouterError> {
        // Block on incoming events if there are no ready connections for consumption
        if self.consume().is_none() {
            // trace!("{}:: {:20} {:20} {:?}", self.id, "", "done-await", self.readyqueue);
            let (id, data) = self.router_rx.recv()?;
            self.events(id, data);
        }

        // Try reading more from connections in a non-blocking
        // fashion to accumulate data and handle subscriptions.
        // Accumulating more data lets requests retrieve bigger
        // bulks which in turn increases efficiency
        for _ in 0..500 {
            // All these methods will handle state and errors
            match self.router_rx.try_recv() {
                Ok((id, data)) => self.events(id, data),
                Err(TryRecvError::Disconnected) => return Err(RouterError::Disconnected),
                Err(TryRecvError::Empty) => break,
            }
        }

        // A connection should not be scheduled multiple times
        debug_assert!(self.scheduler.check_readyqueue_duplicates());

        // Poll 100 connections which are ready in ready queue
        for _ in 0..100 {
            self.consume();
        }

        Ok(())
    }

    fn events(&mut self, id: ConnectionId, data: Event) {
        match data {
            Event::Connect {
                connection,
                incoming,
                outgoing,
            } => self.handle_new_connection(connection, incoming, outgoing),
            Event::DeviceData => self.handle_device_payload(id),
            Event::Disconnect(disconnect) => self.handle_disconnection(id, disconnect.execute_will),
            Event::Ready => self.scheduler.reschedule(id, ScheduleReason::Ready),
            Event::Shadow(request) => {
                retrieve_shadow(&mut self.datalog, &mut self.obufs[id], request)
            }
            Event::Metrics(metrics) => retrieve_metrics(id, self, metrics),
        }
    }

    fn handle_new_connection(
        &mut self,
        mut connection: Connection,
        incoming: Incoming,
        outgoing: Outgoing,
    ) {
        let client_id = outgoing.client_id.clone();

        if self.connections.len() >= self.config.max_connections {
            error!(
                "{:15.15}[E] {:20}",
                client_id, "no space for new connection"
            );
            // let ack = ConnectionAck::Failure("No space for new connection".to_owned());
            // let message = Notification::ConnectionAck(ack);
            return;
        }

        // Retrieve previous connection state from graveyard
        let saved = self.graveyard.retrieve(&client_id);
        let clean_session = connection.clean;
        let previous_session = saved.is_some();
        let tracker = if !clean_session {
            let saved = saved.map_or(SavedState::new(client_id.clone()), |s| s);
            connection.subscriptions = saved.subscriptions;
            connection.meter = saved.metrics;
            saved.tracker
        } else {
            // Only retrieve metrics in clean session
            let saved = saved.map_or(SavedState::new(client_id.clone()), |s| s);
            connection.meter = saved.metrics;
            connection.meter.subscriptions.clear();
            Tracker::new(client_id.clone())
        };
        let ackslog = AckLog::new();

        let time = match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
            Ok(v) => v.as_millis().to_string(),
            Err(e) => format!("Time error = {:?}", e),
        };

        let event = "connection at ".to_owned() + &time + ", clean = " + &clean_session.to_string();
        connection.meter.push_event(event);
        connection
            .meter
            .push_subscriptions(connection.subscriptions.clone());

        let connection_id = self.connections.insert(connection);
        assert_eq!(self.ibufs.insert(incoming), connection_id);
        assert_eq!(self.obufs.insert(outgoing), connection_id);

        self.connection_map.insert(client_id.clone(), connection_id);
        info!(
            "{:15.15}[I] {:20} id = {}",
            client_id, "connect", connection_id
        );

        assert_eq!(self.ackslog.insert(ackslog), connection_id);
        assert_eq!(self.scheduler.add(tracker), connection_id);

        // Check if there are multiple data requests on same filter.
        debug_assert!(self.scheduler.check_tracker_duplicates(connection_id));

        let ack = ConnAck {
            session_present: !clean_session && previous_session,
            code: ConnectReturnCode::Success,
        };

        let ackslog = self.ackslog.get_mut(connection_id).unwrap();
        ackslog.connack(connection_id, ack);

        self.scheduler
            .reschedule(connection_id, ScheduleReason::Init);
    }

    fn handle_disconnection(&mut self, id: ConnectionId, execute_last_will: bool) {
        // Some clients can choose to send Disconnect packet before network disconnection.
        // This will lead to double Disconnect packets in router `events`
        let client_id = match &self.obufs.get(id) {
            Some(v) => v.client_id.clone(),
            None => {
                error!(
                    "{:15.15}[E] {:20} id {} is already gone",
                    "", "no-connection", id
                );
                return;
            }
        };
        if execute_last_will {
            self.handle_last_will(id, client_id.clone());
        }

        info!("{:15.15}[I] {:20} id = {}", client_id, "disconnect", id);

        // Remove connection from router
        let mut connection = self.connections.remove(id);
        let _incoming = self.ibufs.remove(id);
        let _outgoing = self.obufs.remove(id);
        let mut tracker = self.scheduler.remove(id);
        self.connection_map.remove(&client_id);
        self.ackslog.remove(id);

        // Don't remove connection id from readyqueue with index. This will
        // remove wrong connection from readyqueue. Instead just leave diconnected
        // connection in readyqueue and allow 'consume()' method to deal with this
        // self.readyqueue.remove(id);

        let inflight_data_requests = self.datalog.clean(id);

        // Remove this connection from subscriptions
        for filter in connection.subscriptions.iter() {
            if let Some(connections) = self.subscription_map.get_mut(filter) {
                connections.remove(&id);
            }
        }

        // Add disconnection event to metrics
        let time = match SystemTime::now().duration_since(SystemTime::UNIX_EPOCH) {
            Ok(v) => v.as_millis().to_string(),
            Err(e) => format!("Time error = {:?}", e),
        };

        let event = "disconnection at ".to_owned() + &time;
        connection.meter.push_event(event);

        // Save state for persistent sessions
        if !connection.clean {
            // Add inflight data requests back to tracker
            inflight_data_requests
                .into_iter()
                .for_each(|r| tracker.register_data_request(r));

            self.graveyard
                .save(tracker, connection.subscriptions, connection.meter);
        } else {
            // Only save metrics in clean session
            connection.meter.subscriptions.clear();
            self.graveyard
                .save(Tracker::new(client_id), HashSet::new(), connection.meter);
        }
    }

    /// Handles new incoming data on a topic
    fn handle_device_payload(&mut self, id: ConnectionId) {
        // TODO: Retun errors and move error handling to the caller
        let incoming = match self.ibufs.get_mut(id) {
            Some(v) => v,
            None => {
                debug!(
                    "{:15.15}[E] {:20} id {} is already gone",
                    "", "no-connection", id
                );
                return;
            }
        };

        let client_id = incoming.client_id.clone();
        // Instead of exchanging, we should just append new incoming packets inside cache
        let mut packets = incoming.exchange(self.cache.take().unwrap());

        let mut force_ack = false;
        let mut new_data = false;
        let mut disconnect = false;
        let mut execute_will = true;

        // info!("{:15.15}[I] {:20} count = {}", client_id, "packets", packets.len());

        for packet in packets.drain(0..) {
            match packet {
                Packet::Publish(publish, _) => {
                    trace!(
                        "{:15.15}[I] {:20} {:?}",
                        client_id,
                        "publish",
                        publish.topic
                    );

                    let size = publish.len();
                    let qos = publish.qos;
                    let pkid = publish.pkid;

                    // Prepare acks for the above publish
                    // If any of the publish in the batch results in force flush,
                    // set global force flush flag. Force flush is triggered when the
                    // router is in instant ack more or connection data is from a replica
                    //
                    // TODO: handle multiple offsets
                    //
                    // The problem with multiple offsets is that when using replication with the current
                    // architecture, a single publish might get appended to multiple commit logs, resulting in
                    // multiple offsets (see `append_to_commitlog` function), meaning replicas will need to
                    // coordinate using multiple offsets, and we don't have any idea how to do so right now.
                    // Currently as we don't have replication, we just use a single offset, even when appending to
                    // multiple commit logs.

                    match qos {
                        QoS::AtLeastOnce => {
                            let puback = PubAck {
                                pkid,
                                reason: PubAckReason::Success,
                            };

                            let ackslog = self.ackslog.get_mut(id).unwrap();
                            ackslog.puback(puback);
                            force_ack = true;
                        }
                        QoS::ExactlyOnce => {
                            let pubrec = PubRec {
                                pkid,
                                reason: PubRecReason::Success,
                            };

                            let ackslog = self.ackslog.get_mut(id).unwrap();
                            ackslog.pubrec(publish, pubrec);
                            force_ack = true;
                            continue;
                        }
                        QoS::AtMostOnce => {
                            // Do nothing
                        }
                    };

                    self.router_metrics.total_publishes += 1;

                    // Try to append publish to commitlog
                    match append_to_commitlog(
                        id,
                        publish,
                        &mut self.datalog,
                        &mut self.notifications,
                        &mut self.connections,
                    ) {
                        Ok(_offset) => {
                            // Even if one of the data in the batch is appended to commitlog,
                            // set new data. This triggers notifications to wake waiters.
                            // Don't overwrite this flag to false if it is already true.
                            new_data = true;
                        }
                        Err(e) => {
                            // Disconnect on bad publishes
                            error!(
                                "{:15.15}[E] {:20} error = {:?}",
                                client_id, "append-fail", e
                            );
                            self.router_metrics.failed_publishes += 1;
                            disconnect = true;
                            break;
                        }
                    };

                    // Update metrics
                    if let Some(metrics) = self.connections.get_mut(id).map(|v| &mut v.meter) {
                        metrics.increment_publish_count();
                        metrics.add_publish_size(size);
                    }

                    let meter = &mut self.ibufs.get_mut(id).unwrap().meter;
                    meter.publish_count += 1;
                    meter.total_size += size;

                    // println!("{}, {}", self.router_metrics.total_publishes, pkid);
                }
                Packet::Subscribe(s, _) => {
                    let mut return_codes = Vec::new();
                    let pkid = s.pkid;
                    // let len = s.len();

                    for f in s.filters {
                        info!(
                            "{:15.15}[I] {:20} filter = {}",
                            client_id, "subscribe", f.path
                        );
                        let connection = self.connections.get_mut(id).unwrap();

                        if let Err(e) = validate_subscription(connection, &f) {
                            let id = &self.ibufs[id].client_id;
                            error!("{:15.15}[E] {:20} error = {:?}", id, "bad-subscription", e);
                            disconnect = true;
                            break;
                        }

                        let filter = f.path;
                        let qos = f.qos;

                        // Update metrics
                        connection.meter.push_subscription(filter.clone());

                        let (idx, cursor) = self.datalog.next_native_offset(&filter);
                        self.prepare_filter(id, cursor, idx, filter.clone(), qos as u8);
                        self.datalog
                            .handle_retained_messages(&filter, &mut self.notifications);

                        let code = match qos {
                            QoS::AtMostOnce => SubscribeReasonCode::QoS0,
                            QoS::AtLeastOnce => SubscribeReasonCode::QoS1,
                            QoS::ExactlyOnce => SubscribeReasonCode::QoS2,
                        };

                        return_codes.push(code);
                    }

                    // let meter = &mut self.ibufs.get_mut(id).unwrap().meter;
                    // meter.total_size += len;

                    let suback = SubAck { pkid, return_codes };
                    let ackslog = self.ackslog.get_mut(id).unwrap();
                    ackslog.suback(suback);
                    force_ack = true;
                }
                Packet::Unsubscribe(unsubscribe) => {
                    debug!(
                        "{:11} {:14} Id = {} Filters = {:?}",
                        "data", "unsubscribe", id, unsubscribe.filters
                    );
                    let connection = self.connections.get_mut(id).unwrap();
                    let pkid = unsubscribe.pkid;
                    for filter in unsubscribe.filters {
                        if let Some(connection_ids) = self.subscription_map.get_mut(&filter) {
                            let removed = connection_ids.remove(&id);
                            if !removed {
                                continue;
                            }

                            connection.meter.remove_subscription(filter.clone());
                            let meter = &mut self.ibufs.get_mut(id).unwrap().meter;
                            meter.subscribe_count -= 1;

                            let outgoing = self.obufs.get_mut(id).unwrap();

                            if connection.subscriptions.contains(&filter) {
                                connection.subscriptions.remove(&filter);
                                debug!(
                                    "{:15.15}[I] {:20} filter = {}",
                                    outgoing.client_id, "unsubscribe", filter
                                );
                            } else {
                                error!(
                                    "{:15.15}[E] {:20} pkid = {:?}",
                                    id, "unsubscribe-failed", unsubscribe.pkid
                                );
                                continue;
                            }
                            let unsuback = UnsubAck {
                                pkid,
                                // reasons are used in MQTTv5
                                reasons: vec![],
                            };
                            let ackslog = self.ackslog.get_mut(id).unwrap();
                            ackslog.unsuback(unsuback);
                            self.scheduler.untrack(id, &filter);
                            self.datalog.remove_waiters_for_id(id, &filter);
                            force_ack = true;
                        }
                    }
                }
                Packet::PubAck(puback, _) => {
                    let outgoing = self.obufs.get_mut(id).unwrap();
                    let pkid = puback.pkid;
                    if outgoing.register_ack(pkid).is_none() {
                        error!(
                            "{:15.15}[E] {:20} pkid = {:?}",
                            id, "unsolicited/ooo ack", pkid
                        );
                        disconnect = true;
                        break;
                    }

                    self.scheduler.reschedule(id, ScheduleReason::IncomingAck);
                }
                Packet::PubRec(pubrec, _) => {
                    let outgoing = self.obufs.get_mut(id).unwrap();
                    let pkid = pubrec.pkid;
                    if outgoing.register_ack(pkid).is_none() {
                        error!(
                            "{:15.15}[E] {:20} pkid = {:?}",
                            id, "unsolicited/ooo ack", pkid
                        );
                        disconnect = true;
                        break;
                    }

                    let ackslog = self.ackslog.get_mut(id).unwrap();
                    let pubrel = PubRel {
                        pkid: pubrec.pkid,
                        reason: PubRelReason::Success,
                    };

                    ackslog.pubrel(pubrel);
                    self.scheduler.reschedule(id, ScheduleReason::IncomingAck);
                }
                Packet::PubRel(pubrel, None) => {
                    let ackslog = self.ackslog.get_mut(id).unwrap();
                    let pubcomp = PubComp {
                        pkid: pubrel.pkid,
                        reason: PubCompReason::Success,
                    };

                    let publish = match ackslog.pubcomp(pubcomp) {
                        Some(v) => v,
                        None => {
                            disconnect = true;
                            break;
                        }
                    };

                    // Try to append publish to commitlog
                    match append_to_commitlog(
                        id,
                        publish,
                        &mut self.datalog,
                        &mut self.notifications,
                        &mut self.connections,
                    ) {
                        Ok(_offset) => {
                            // Even if one of the data in the batch is appended to commitlog,
                            // set new data. This triggers notifications to wake waiters.
                            // Don't overwrite this flag to false if it is already true.
                            new_data = true;
                        }
                        Err(e) => {
                            // Disconnect on bad publishes
                            error!(
                                "{:15.15}[E] {:20} error = {:?}",
                                client_id, "append-fail", e
                            );
                            self.router_metrics.failed_publishes += 1;
                            disconnect = true;
                            break;
                        }
                    };
                }
                Packet::PubComp(_pubcomp, _) => {}
                Packet::PingReq(_) => {
                    let ackslog = self.ackslog.get_mut(id).unwrap();
                    ackslog.pingresp(PingResp);

                    force_ack = true;
                }
                Packet::Disconnect => {
                    disconnect = true;
                    execute_will = false;
                    break;
                }
                incoming => {
                    warn!("Packet = {:?} not supported by router yet", incoming);
                }
            }
        }

        self.cache = Some(packets);

        // Prepare AcksRequest in tracker if router is operating in a
        // single node mode or force ack request for subscriptions
        if force_ack {
            self.scheduler.reschedule(id, ScheduleReason::FreshData);
        }

        // Notify waiting consumers only if there is publish data. During
        // subscription, data request is added to data waiter. With out this
        // if condition, connection will be woken up even during subscription
        if new_data {
            // Prepare all the consumers which are waiting for new data
            while let Some((id, request)) = self.notifications.pop_front() {
                self.scheduler.track(id, request);
                self.scheduler.reschedule(id, ScheduleReason::FreshData);
            }
        }

        // Incase BytesMut represents 10 packets, publish error/diconnect event
        // on say 5th packet should not block new data notifications for packets
        // 1 - 4. Hence we use a flag instead of diconnecting immediately
        if disconnect {
            self.handle_disconnection(id, execute_will);
        }
    }

    /// Apply filter and prepare this connection to receive subscription data
    fn prepare_filter(
        &mut self,
        id: ConnectionId,
        cursor: Offset,
        filter_idx: FilterIdx,
        filter: String,
        qos: u8,
    ) {
        // Add connection id to subscription list
        match self.subscription_map.get_mut(&filter) {
            Some(connections) => {
                connections.insert(id);
            }
            None => {
                let mut connections = HashSet::new();
                connections.insert(id);
                self.subscription_map.insert(filter.clone(), connections);
            }
        }

        // Prepare consumer to pull data in case of subscription
        let connection = self.connections.get_mut(id).unwrap();

        if !connection.subscriptions.contains(&filter) {
            connection.subscriptions.insert(filter.clone());
            let request = DataRequest {
                filter,
                filter_idx,
                qos,
                cursor,
                read_count: 0,
                max_count: 100,
            };

            self.scheduler.track(id, request);
            self.scheduler.reschedule(id, ScheduleReason::NewFilter);
            debug_assert!(self.scheduler.check_tracker_duplicates(id))
        }

        let meter = &mut self.ibufs.get_mut(id).unwrap().meter;
        meter.subscribe_count += 1;
    }

    /// When a connection is ready, it should sweep native data from 'datalog',
    /// send data and notifications to consumer.
    /// To activate a connection, first connection's tracker is fetched and
    /// all the requests are handled.
    fn consume(&mut self) -> Option<()> {
        let (id, mut requests) = self.scheduler.poll()?;

        let outgoing = match self.obufs.get_mut(id) {
            Some(v) => v,
            None => {
                debug!(
                    "{:15.15}[E] {:20} id {} is already gone",
                    "", "no-connection", id
                );
                return Some(());
            }
        };

        let ackslog = self.ackslog.get_mut(id).unwrap();
        let datalog = &mut self.datalog;

        trace!(
            "{:15.15}[S] {:20} id = {}",
            outgoing.client_id,
            "consume",
            id
        );

        // We always try to ack when ever a connection is scheduled
        if ack_device_data(ackslog, outgoing) {
            trace!("{:15.15}[T] {:20}", outgoing.client_id, "acks-done");
        }

        // A new connection's tracker is always initialized with acks request.
        // A subscribe will register data request.
        // So a new connection is always scheduled with at least one request
        for _ in 0..MAX_SCHEDULE_ITERATIONS {
            let mut request = match requests.pop_front() {
                // Handle next data or acks request
                Some(request) => request,
                // No requests in the queue. This implies that consumer data and
                // acks are completely caught up. Pending requests are registered
                // in waiters and awaiting new notifications (device or replica data)
                None => {
                    self.scheduler.pause(id, PauseReason::Caughtup);
                    return Some(());
                }
            };

            match forward_device_data(&mut request, datalog, outgoing) {
                ConsumeStatus::BufferFull => {
                    requests.push_back(request);
                    self.scheduler.pause(id, PauseReason::Busy);
                    break;
                }
                ConsumeStatus::InflightFull => {
                    requests.push_back(request);
                    self.scheduler.pause(id, PauseReason::InflightFull);
                    break;
                }
                ConsumeStatus::FilterCaughtup => {
                    let filter = &request.filter;
                    trace!(
                        "{:15.15}[S] {:20} f = {filter}",
                        outgoing.client_id,
                        "caughtup-park"
                    );

                    // When all the data in the log is caught up, current request is
                    // registered in waiters and not added back to the tracker. This
                    // ensures that tracker.next() stops when all the requests are done
                    datalog.park(id, request);
                }
                ConsumeStatus::PartialRead => {
                    requests.push_back(request);
                }
            }
        }

        // Add requests back to the tracker if there are any
        self.scheduler.trackv(id, requests);
        Some(())
    }

    pub fn handle_last_will(&mut self, id: ConnectionId, client_id: String) {
        let connection = self.connections.get_mut(id).unwrap();
        let will = match connection.last_will.take() {
            Some(v) => v,
            None => return,
        };

        let publish = Publish {
            dup: false,
            qos: will.qos,
            retain: will.retain,
            topic: will.topic,
            pkid: 0,
            payload: will.message,
        };
        match append_to_commitlog(
            id,
            publish,
            &mut self.datalog,
            &mut self.notifications,
            &mut self.connections,
        ) {
            Ok(_offset) => {
                // Prepare all the consumers which are waiting for new data
                while let Some((id, request)) = self.notifications.pop_front() {
                    self.scheduler.track(id, request);
                    self.scheduler.reschedule(id, ScheduleReason::FreshData);
                }
            }
            Err(e) => {
                // Disconnect on bad publishes
                error!(
                    "{:15.15}[E] {:20} error = {:?}",
                    client_id, "append-fail", e
                );
                self.router_metrics.failed_publishes += 1;
                // Removed disconnect = true from here because we disconnect anyways
            }
        };
    }
}

fn append_to_commitlog(
    id: ConnectionId,
    mut publish: Publish,
    datalog: &mut DataLog,
    notifications: &mut VecDeque<(ConnectionId, DataRequest)>,
    connections: &mut Slab<Connection>,
) -> Result<Offset, RouterError> {
    let topic = std::str::from_utf8(&publish.topic)?;

    // Ensure that only clients associated with a tenant can publish to tenant's topic
    if let Some(tenant_prefix) = &connections[id].tenant_prefix {
        if !topic.starts_with(tenant_prefix) {
            return Err(RouterError::BadTenant(
                tenant_prefix.to_owned(),
                topic.to_owned(),
            ));
        }
    }

    if publish.payload.is_empty() {
        datalog.remove_from_retained_publishes(topic.to_owned());
    } else if publish.retain {
        datalog.insert_to_retained_publishes(publish.clone(), topic.to_owned());
    }

    publish.retain = false;
    let pkid = publish.pkid;

    let filter_idxs = datalog.matches(topic);

    // Create a dynamic filter if dynamic_filters are enabled for this connection
    let filter_idxs = match filter_idxs {
        Some(v) => v,
        None if connections[id].dynamic_filters => {
            let mut filter_idxs = vec![];
            let (idx, _cursor) = datalog.next_native_offset(topic);
            filter_idxs.push(idx);
            filter_idxs
        }
        None => return Err(RouterError::NoMatchingFilters(topic.to_owned())),
    };

    let mut o = (0, 0);
    for filter_idx in filter_idxs {
        let datalog = datalog.native.get_mut(filter_idx).unwrap();
        let (offset, filter) = datalog.append(publish.clone(), notifications);
        debug!(
            "{:15.15}[I] {:20} append = {}[{}, {}), pkid = {}",
            // map client id from connection id
            connections[id].client_id,
            "publish",
            filter,
            offset.0,
            offset.1,
            pkid
        );

        o = offset;
    }

    // error!("{:15.15}[E] {:20} topic = {}", connections[id].client_id, "no-filter", topic);
    Ok(o)
}

/// Sweep ackslog for all the pending acks.
/// We write everything to outgoing buf with out worrying about buffer size
/// because acks most certainly won't cause memory bloat
fn ack_device_data(ackslog: &mut AckLog, outgoing: &mut Outgoing) -> bool {
    let acks = ackslog.readv();
    if acks.is_empty() {
        return true;
    }

    let mut count = 0;
    let mut buffer = outgoing.data_buffer.lock();

    // Unlike forwards, we are reading all the pending acks for a given connection.
    // At any given point of time, there can be a max of connection's buffer size
    for ack in acks.drain(..) {
        let pkid = packetid(&ack);
        trace!(
            "{:15.15}[O] {:20} pkid = {:?}",
            outgoing.client_id,
            "ack",
            pkid
        );
        let message = Notification::DeviceAck(ack);
        buffer.push_back(message);
        count += 1;
    }

    debug!(
        "{:15.15}[O] {:20} count = {:?}",
        outgoing.client_id, "acks", count
    );
    outgoing.handle.try_send(()).ok();
    true
}

enum ConsumeStatus {
    BufferFull,
    InflightFull,
    FilterCaughtup,
    PartialRead,
}

/// Sweep datalog from offset in DataRequest and updates DataRequest
/// for next sweep. Returns (busy, caughtup) status
/// Returned arguments:
/// 1. `busy`: whether the data request was completed or not.
/// 2. `done`: whether the connection was busy or not.
/// 3. `inflight_full`: whether the inflight requests were completely filled
fn forward_device_data(
    request: &mut DataRequest,
    datalog: &DataLog,
    outgoing: &mut Outgoing,
) -> ConsumeStatus {
    trace!(
        "{:15.15}[T] {:20} cursor = {}[{}, {}]",
        outgoing.client_id,
        "data-request",
        request.filter,
        request.cursor.0,
        request.cursor.1
    );

    let inflight_slots = if request.qos == 1 {
        let len = outgoing.free_slots();
        if len == 0 {
            return ConsumeStatus::InflightFull;
        }

        len as u64
    } else {
        datalog.config.max_read_len
    };

    let (next, publishes) =
        match datalog.native_readv(request.filter_idx, request.cursor, inflight_slots) {
            Ok(v) => v,
            Err(e) => {
                error!("Failed to read from commitlog. Error = {:?}", e);
                return ConsumeStatus::FilterCaughtup;
            }
        };

    let (start, next, caughtup) = match next {
        Position::Next { start, end } => (start, end, false),
        Position::Done { start, end } => (start, end, true),
    };

    if start != request.cursor {
        error!(
            "Read cursor jump. Cursor = {:?}, Start = {:?}",
            request.cursor, start
        );
    }

    trace!(
        "{:15.15}[T] {:20} cursor = {}[{}, {})",
        outgoing.client_id,
        "data-response",
        request.filter,
        next.0,
        next.1,
    );

    let qos = request.qos;
    let filter_idx = request.filter_idx;
    request.read_count += publishes.len();
    request.cursor = next;
    // println!("{:?} {:?} {}", start, next, request.read_count);

    if publishes.is_empty() {
        return ConsumeStatus::FilterCaughtup;
    }

    // Fill and notify device data
    debug!(
        "{:15.15}[O] {:20} cursor = {}[{}, {}) count = {}",
        outgoing.client_id,
        "data-proxy",
        request.filter,
        request.cursor.0,
        request.cursor.1,
        publishes.len()
    );

    let forwards = publishes.into_iter().map(|mut publish| {
        publish.qos = protocol::qos(qos).unwrap();
        Forward {
            cursor: next,
            size: 0,
            publish,
        }
    });

    let (len, inflight) = outgoing.push_forwards(forwards, qos, filter_idx);

    trace!(
        "{:15.15}[O] {:20} buffer = {}, inflight = {}",
        outgoing.client_id,
        "inflight",
        len,
        inflight
    );

    if len >= MAX_CHANNEL_CAPACITY - 1 {
        outgoing.push_notification(Notification::Unschedule);
        outgoing.handle.try_send(()).ok();
        return ConsumeStatus::BufferFull;
    }

    outgoing.handle.try_send(()).ok();
    if caughtup {
        ConsumeStatus::FilterCaughtup
    } else {
        ConsumeStatus::PartialRead
    }
}

fn retrieve_shadow(datalog: &mut DataLog, outgoing: &mut Outgoing, shadow: ShadowRequest) {
    if let Some(reply) = datalog.shadow(&shadow.filter) {
        let publish = reply;
        let shadow_reply = router::ShadowReply {
            topic: publish.topic,
            payload: publish.payload,
        };

        // FIll notify shadow
        let message = Notification::Shadow(shadow_reply);
        let len = outgoing.push_notification(message);
        let _should_unschedule = if len >= MAX_CHANNEL_CAPACITY - 1 {
            outgoing.push_notification(Notification::Unschedule);
            true
        } else {
            false
        };
        outgoing.handle.try_send(()).ok();
    }
}

fn retrieve_metrics(id: ConnectionId, router: &mut Router, metrics: MetricsRequest) {
    let message = match metrics {
        MetricsRequest::Config => MetricsReply::Config(router.config.clone()),
        MetricsRequest::Router => MetricsReply::Router(router.router_metrics.clone()),
        MetricsRequest::Connection(id) => {
            let metrics = router.connection_map.get(&id).map(|v| {
                let c = router.connections.get(*v).map(|v| v.meter.clone()).unwrap();
                let t = router.scheduler.trackers.get(*v).cloned().unwrap();
                (c, t)
            });

            let metrics = match metrics {
                Some(v) => Some(v),
                None => router
                    .graveyard
                    .retrieve(&id)
                    .map(|v| (v.metrics, v.tracker)),
            };

            MetricsReply::Connection(metrics)
        }
        MetricsRequest::Subscriptions => {
            let metrics: HashMap<Filter, Vec<String>> = router
                .subscription_map
                .iter()
                .map(|(filter, connections)| {
                    let connections = connections
                        .iter()
                        .map(|id| router.obufs[*id].client_id.clone())
                        .collect();

                    (filter.to_owned(), connections)
                })
                .collect();

            MetricsReply::Subscriptions(metrics)
        }
        MetricsRequest::Subscription(filter) => {
            let metrics = router.datalog.meter(&filter);
            MetricsReply::Subscription(metrics)
        }
        MetricsRequest::Waiters(filter) => {
            let metrics = router.datalog.waiters(&filter).map(|v| {
                // Convert (connection id, data request) list to (device id, data request) list
                v.waiters()
                    .iter()
                    .map(|(id, request)| (router.obufs[*id].client_id.clone(), request.clone()))
                    .collect()
            });

            MetricsReply::Waiters(metrics)
        }
        MetricsRequest::ReadyQueue => {
            let metrics = router.scheduler.readyqueue.clone();
            MetricsReply::ReadyQueue(metrics)
        }
    };

    let connection = router.connections.get_mut(id).unwrap();
    connection.metrics.try_send(message).ok();
}

fn validate_subscription(
    connection: &mut Connection,
    filter: &protocol::Filter,
) -> Result<(), RouterError> {
    // Ensure that only client devices of the tenant can
    if let Some(tenant_prefix) = &connection.tenant_prefix {
        if !filter.path.starts_with(tenant_prefix) {
            return Err(RouterError::InvalidFilterPrefix(filter.path.to_owned()));
        }
    }

    if filter.qos == QoS::ExactlyOnce {
        return Err(RouterError::UnsupportedQoS(filter.qos));
    }

    if filter.path.starts_with("test") || filter.path.starts_with('$') {
        return Err(RouterError::InvalidFilterPrefix(filter.path.to_owned()));
    }

    Ok(())
}

// #[cfg(test)]
// #[allow(non_snake_case)]
// mod test {
//     use std::{
//         thread,
//         time::{Duration, Instant},
//     };

//     use bytes::BytesMut;

//     use super::*;
//     use crate::{
//         link::local::*,
//         protocol::v4::{self, subscribe::SubscribeFilter, QoS},
//         router::ConnectionMeter,
//     };

//     /// Create a router and n connections
//     fn new_router(count: usize, clean: bool) -> (Router, VecDeque<(LinkTx, LinkRx)>) {
//         let config = RouterConfig {
//             instant_ack: true,
//             max_segment_size: 1024 * 10, // 10 KB
//             max_mem_segments: 10,
//             max_disk_segments: 0,
//             max_read_len: 128,
//             log_dir: None,
//             max_connections: 128,
//             dynamic_log: false,
//         };

//         let mut router = Router::new(0, config);
//         let link = router.link();
//         let handle = thread::spawn(move || {
//             (0..count)
//                 .map(|i| Link::new(&format!("link-{}", i), link.clone(), clean))
//                 .collect::<VecDeque<Result<_, _>>>()
//         });

//         router.run_exact(count).unwrap();
//         let links = handle
//             .join()
//             .unwrap()
//             .into_iter()
//             .map(|x| x.unwrap())
//             .collect();
//         (router, links)
//     }

//     fn reconnect(router: &mut Router, link_id: usize, clean: bool) -> (LinkTx, LinkRx) {
//         let link = router.link();
//         let handle =
//             thread::spawn(move || Link::new(&format!("link-{}", link_id), link, clean).unwrap());
//         router.run(1).unwrap();
//         handle.join().unwrap()
//     }

//     #[test]
//     fn test_graveyard_retreive_metrics_always() {
//         let (mut router, mut links) = new_router(1, false);
//         let (tx, _) = links.pop_front().unwrap();
//         let id = tx.connection_id;
//         let conn = router.connections.get_mut(id).unwrap();
//         conn.meter = ConnectionMeter {
//             publish_size: 1000,
//             publish_count: 1000,
//             subscriptions: HashSet::new(),
//             events: conn.meter.events.clone(),
//         };
//         conn.meter.push_event("graveyard-testing".to_string());
//         router.handle_disconnection(id);

//         let (tx, _) = reconnect(&mut router, 0, false);
//         let id = tx.connection_id;
//         let conn = router.connections.get_mut(id).unwrap();
//         assert_eq!(conn.meter.publish_size, 1000);
//         assert_eq!(conn.meter.publish_count, 1000);
//         assert_eq!(conn.meter.events.len(), 4);

//         let (mut router, mut links) = new_router(1, true);
//         let (tx, _) = links.pop_front().unwrap();
//         let id = tx.connection_id;
//         let conn = router.connections.get_mut(id).unwrap();
//         conn.meter = ConnectionMeter {
//             publish_size: 1000,
//             publish_count: 1000,
//             subscriptions: HashSet::new(),
//             events: conn.meter.events.clone(),
//         };
//         conn.meter.push_event("graveyard-testing".to_string());
//         router.handle_disconnection(id);

//         let (tx, _) = reconnect(&mut router, 0, true);
//         let id = tx.connection_id;
//         let conn = router.connections.get_mut(id).unwrap();
//         assert_eq!(conn.meter.publish_size, 1000);
//         assert_eq!(conn.meter.publish_count, 1000);
//         assert_eq!(conn.meter.events.len(), 4);
//     }

//     #[test]
//     #[should_panic]
//     fn test_blocking_too_many_connections() {
//         new_router(512, true);
//     }

//     #[test]
//     fn test_not_clean_sessions() {
//         // called once per data push (subscribe to 2 topics in our case)
//         // called once per each sub topic
//         // if prepare_data_request called, then Tracker::register_data_request is also called so
//         // no need to check separately for that

//         let (mut router, mut links) = new_router(2, false);
//         let (tx1, mut rx1) = links.pop_front().unwrap();
//         let id1 = tx1.connection_id;
//         let (tx2, mut rx2) = links.pop_front().unwrap();
//         let id2 = tx2.connection_id;

//         // manually subscribing to avoid calling Router::run(), so that we can see and test changes
//         // in Router::readyqueue.
//         let mut buf = BytesMut::new();
//         v4::subscribe::write(
//             vec![
//                 SubscribeFilter::new("hello/1/world".to_string(), QoS::AtMostOnce),
//                 SubscribeFilter::new("hello/2/world".to_string(), QoS::AtMostOnce),
//             ],
//             0,
//             &mut buf,
//         )
//         .unwrap();
//         let buf = buf.freeze();
//         {
//             let incoming = router.ibufs.get_mut(id1).unwrap();
//             let mut recv_buf = incoming.buffer.lock();
//             recv_buf.push_back(buf.clone());
//             assert_eq!(router.subscription_map.len(), 0);
//         }
//         {
//             let incoming = router.ibufs.get_mut(id2).unwrap();
//             let mut recv_buf = incoming.buffer.lock();
//             recv_buf.push_back(buf.clone());
//             assert_eq!(router.subscription_map.len(), 0);
//         }

//         router.handle_device_payload(id1);
//         assert_eq!(router.subscription_map.len(), 2);
//         assert_eq!(router.readyqueue.len(), 1);
//         router.consume(id1);
//         assert_eq!(
//             router
//                 .datalog
//                 .waiters(&"hello/1/world".to_string())
//                 .unwrap()
//                 .waiters()
//                 .len(),
//             1
//         );
//         assert_eq!(
//             router
//                 .datalog
//                 .waiters(&"hello/2/world".to_string())
//                 .unwrap()
//                 .waiters()
//                 .len(),
//             1
//         );

//         router.handle_device_payload(id2);
//         assert_eq!(router.subscription_map.len(), 2);
//         assert_eq!(router.readyqueue.len(), 2);
//         router.consume(id2);
//         assert_eq!(
//             router
//                 .datalog
//                 .waiters(&"hello/1/world".to_string())
//                 .unwrap()
//                 .waiters()
//                 .len(),
//             2
//         );
//         assert_eq!(
//             router
//                 .datalog
//                 .waiters(&"hello/2/world".to_string())
//                 .unwrap()
//                 .waiters()
//                 .len(),
//             2
//         );

//         assert!(matches!(rx1.recv().unwrap(), Some(Notification::DeviceAck(_))));
//         assert!(matches!(rx2.recv().unwrap(), Some(Notification::DeviceAck(_))));

//         router.handle_disconnection(id1);
//         router.handle_disconnection(id2);
//         router.run(1).unwrap();

//         let (_, _) = reconnect(&mut router, 0, false);
//         assert_eq!(
//             router
//                 .datalog
//                 .waiters(&"hello/1/world".to_string())
//                 .unwrap()
//                 .waiters()
//                 .len(),
//             1
//         );
//         assert_eq!(
//             router
//                 .datalog
//                 .waiters(&"hello/2/world".to_string())
//                 .unwrap()
//                 .waiters()
//                 .len(),
//             1
//         );

//         let (_, _) = reconnect(&mut router, 1, false);
//         assert_eq!(
//             router
//                 .datalog
//                 .waiters(&"hello/1/world".to_string())
//                 .unwrap()
//                 .waiters()
//                 .len(),
//             2
//         );
//         assert_eq!(
//             router
//                 .datalog
//                 .waiters(&"hello/2/world".to_string())
//                 .unwrap()
//                 .waiters()
//                 .len(),
//             2
//         );
//     }

//     #[test]
//     fn test_publish_appended_to_commitlog() {
//         let (mut router, mut links) = new_router(2, true);
//         let (mut sub_tx, _) = links.pop_front().unwrap();
//         let (mut pub_tx, _) = links.pop_front().unwrap();

//         sub_tx.subscribe("hello/1/world").unwrap();
//         router.run(1).unwrap();

//         // Each LinkTx::publish creates a single new event
//         for _ in 0..10 {
//             pub_tx.publish("hello/1/world", b"hello".to_vec()).unwrap();
//         }
//         router.run(1).unwrap();
//         assert_eq!(router.datalog.next_native_offset("hello/1/world").1, (0, 10));
//     }

//     #[test]
//     fn test_adding_caughtups_to_waiters() {
//         // number of times forward_device_data hit
//         //     = router.run times * topics * subscribers per topic
//         //     = 2 * 2 * 2 = 8
//         //
//         // we also call consume 2 times per subsriber, and one time it will call
//         // forward_device_data again, but no data will actually be forwarded.
//         // as 256 publishes at once, and router's config only reads 128 at a time, data needs to be
//         // read from same log twice, and thus half of the times the data reading is not done

//         let (mut router, mut links) = new_router(4, true);
//         let (mut sub1_tx, _) = links.pop_front().unwrap();
//         let (mut pub1_tx, _) = links.pop_front().unwrap();
//         let (mut sub2_tx, _) = links.pop_front().unwrap();
//         let (mut pub2_tx, _) = links.pop_front().unwrap();
//         sub1_tx.subscribe("hello/1/world").unwrap();
//         sub1_tx.subscribe("hello/2/world").unwrap();
//         sub2_tx.subscribe("hello/1/world").unwrap();
//         sub2_tx.subscribe("hello/2/world").unwrap();
//         router.run(1).unwrap();

//         assert_eq!(
//             router
//                 .datalog
//                 .waiters(&"hello/1/world".to_string())
//                 .unwrap()
//                 .waiters()
//                 .len(),
//             2
//         );
//         assert_eq!(
//             router
//                 .datalog
//                 .waiters(&"hello/2/world".to_string())
//                 .unwrap()
//                 .waiters()
//                 .len(),
//             2
//         );

//         for _ in 0..256 {
//             pub1_tx.publish("hello/1/world", b"hello".to_vec()).unwrap();
//             pub2_tx.publish("hello/2/world", b"hello".to_vec()).unwrap();
//         }
//         router.run(1).unwrap();

//         let tracker = router.trackers.get(sub1_tx.connection_id).unwrap();
//         assert_eq!(tracker.get_data_requests().len(), 2);
//         let tracker = router.trackers.get(sub2_tx.connection_id).unwrap();
//         assert_eq!(tracker.get_data_requests().len(), 2);

//         router.run(1).unwrap();

//         let tracker = router.trackers.get(sub1_tx.connection_id).unwrap();
//         assert_eq!(tracker.get_data_requests().len(), 2);
//         let tracker = router.trackers.get(sub2_tx.connection_id).unwrap();
//         assert_eq!(tracker.get_data_requests().len(), 2);

//         router.consume(sub1_tx.connection_id);
//         router.consume(sub2_tx.connection_id);

//         let tracker = router.trackers.get(sub1_tx.connection_id).unwrap();
//         assert_eq!(tracker.get_data_requests().len(), 1);
//         let tracker = router.trackers.get(sub2_tx.connection_id).unwrap();
//         assert_eq!(tracker.get_data_requests().len(), 1);

//         router.consume(sub1_tx.connection_id);
//         router.consume(sub2_tx.connection_id);

//         let tracker = router.trackers.get(sub1_tx.connection_id).unwrap();
//         assert_eq!(tracker.get_data_requests().len(), 0);
//         let tracker = router.trackers.get(sub2_tx.connection_id).unwrap();
//         assert_eq!(tracker.get_data_requests().len(), 0);

//         assert_eq!(
//             router
//                 .datalog
//                 .waiters(&"hello/1/world".to_string())
//                 .unwrap()
//                 .waiters()
//                 .len(),
//             2
//         );
//         assert_eq!(
//             router
//                 .datalog
//                 .waiters(&"hello/2/world".to_string())
//                 .unwrap()
//                 .waiters()
//                 .len(),
//             2
//         );
//     }

//     #[test]
//     fn test_disconnect_invalid_sub() {
//         let (mut router, mut links) = new_router(3, true);

//         let (mut tx, mut rx) = links.pop_front().unwrap();
//         tx.subscribe("$hello/world").unwrap();
//         router.run(1).unwrap();
//         assert!(matches!(rx.recv(), Err(LinkError::Recv(flume::RecvError::Disconnected))));

//         let (mut tx, mut rx) = links.pop_front().unwrap();
//         tx.subscribe("test/hello").unwrap();
//         router.run(1).unwrap();
//         assert!(matches!(rx.recv(), Err(LinkError::Recv(flume::RecvError::Disconnected))));
//     }

//     #[test]
//     fn test_disconnect_invalid_pub() {
//         let (mut router, mut links) = new_router(1, true);
//         let (mut tx, mut rx) = links.pop_front().unwrap();
//         tx.publish("invalid/topic", b"hello".to_vec()).unwrap();
//         router.run(1).unwrap();
//         assert!(matches!(rx.recv(), Err(LinkError::Recv(flume::RecvError::Disconnected))));
//     }

//     #[test]
//     fn test_pingreq() {
//         let (mut router, mut links) = new_router(1, true);
//         let (mut tx, mut rx) = links.pop_front().unwrap();
//         let mut buf = BytesMut::new();
//         mqttbytes::v4::PingReq.write(&mut buf).unwrap();
//         let buf = buf.freeze();
//         for _ in 0..10 {
//             tx.push(buf.clone()).unwrap();
//         }
//         router.run(1).unwrap();
//         for _ in 0..10 {
//             let ret = rx.recv().unwrap().unwrap();
//             assert!(matches!(ret, Notification::DeviceAck(Ack::PingResp)));
//         }
//     }

//     #[test]
//     fn test_disconnect() {
//         let (mut router, mut links) = new_router(1, true);
//         let (mut tx, mut rx) = links.pop_front().unwrap();
//         let mut buf = BytesMut::new();
//         mqttbytes::v4::Disconnect.write(&mut buf).unwrap();
//         let buf = buf.freeze();
//         tx.push(buf).unwrap();
//         router.run(1).unwrap();
//         assert!(matches!(rx.recv(), Err(LinkError::Recv(flume::RecvError::Disconnected))));
//     }

//     #[test]
//     fn test_connection_in_qos1() {
//         // connection as in crate::connection::Connection struct should get filled for all unacked,
//         // and refuse to add more packets than unacked.

//         let config = RouterConfig {
//             instant_ack: true,
//             max_segment_size: 1024 * 10, // 10 KB
//             max_mem_segments: 10,
//             max_disk_segments: 0,
//             max_read_len: 256,
//             log_dir: None,
//             max_connections: 128,
//             dynamic_log: false,
//         };

//         let mut router = Router::new(0, config);
//         let link = router.link();
//         let handle = thread::spawn(move || {
//             (0..2)
//                 .map(|i| Link::new(&format!("link-{}", i), link.clone(), true))
//                 .collect::<VecDeque<Result<_, _>>>()
//         });

//         router.run(2).unwrap();
//         let mut links: VecDeque<(LinkTx, LinkRx)> = handle
//             .join()
//             .unwrap()
//             .into_iter()
//             .map(|x| x.unwrap())
//             .collect();

//         let (mut sub_tx, _sub_rx) = links.pop_front().unwrap();
//         let (mut pub_tx, _) = links.pop_front().unwrap();

//         let mut buf = BytesMut::new();
//         mqttbytes::v4::Subscribe::new("hello/1/world", mqttbytes::QoS::AtLeastOnce)
//             .write(&mut buf)
//             .unwrap();
//         let buf = buf.freeze();
//         sub_tx.push(buf).unwrap();
//         router.run(1).unwrap();

//         for _ in 0..202 {
//             pub_tx.publish("hello/1/world", b"hello".to_vec()).unwrap();
//         }
//         router.run(1).unwrap();

//         let id = sub_tx.connection_id;
//         let tracker = router.trackers.get(id).unwrap();
//         assert_eq!(tracker.get_data_requests().front().unwrap().cursor, (0, 200));
//     }

//     #[test]
//     #[ignore]
//     fn test_resend_in_qos1() {
//         unimplemented!("we don't resend QoS1 packets yet")
//     }

//     #[test]
//     fn test_wildcard_subs() {
//         let (mut router, mut links) = new_router(3, true);

//         let (mut sub1_tx, mut sub1_rx) = links.pop_front().unwrap();
//         sub1_tx.subscribe("#").unwrap();

//         let (mut sub2_tx, mut sub2_rx) = links.pop_front().unwrap();
//         sub2_tx.subscribe("hello/+/world").unwrap();

//         let (mut pub_tx, mut _pub_rx) = links.pop_front().unwrap();
//         for _ in 0..10 {
//             for i in 0..10 {
//                 pub_tx
//                     .publish(format!("hello/{}/world", i), b"hello".to_vec())
//                     .unwrap();
//             }
//         }

//         for _ in 0..10 {
//             pub_tx.publish("hello/world", b"hello".to_vec()).unwrap();
//         }

//         router.run(1).unwrap();

//         let mut count = 0;
//         while let Ok(Some(notification)) =
//             sub1_rx.recv_deadline(Instant::now() + Duration::from_millis(1))
//         {
//             match notification {
//                 Notification::Forward { .. } => count += 1,
//                 _ => {}
//             }
//         }
//         assert_eq!(count, 110);

//         count = 0;
//         while let Ok(Some(notification)) =
//             sub2_rx.recv_deadline(Instant::now() + Duration::from_millis(1))
//         {
//             match notification {
//                 Notification::Forward { .. } => count += 1,
//                 _ => {}
//             }
//         }
//         assert_eq!(count, 100);
//     }
// }

// // #[cfg(test)]
// // mod test {
// //     use crate::connection::Connection;
// //     use crate::link::local::{Link, LinkRx, LinkTx};
// //     use crate::tracker::Tracker;
// //     use crate::{ConnectionId, Notification, Router, RouterConfig};
// //     use bytes::BytesMut;
// //     use flume::Receiver;
// //     use mqttbytes::v4::{Publish, Subscribe};
// //     use mqttbytes::QoS;
// //     use std::collections::VecDeque;
// //     use std::sync::atomic::AtomicBool;
// //     use std::sync::Arc;
// //     use std::thread;

// //     /// Create a router and n connections
// //     fn router(count: usize) -> (Router, VecDeque<(LinkTx, LinkRx)>) {
// //         let config = RouterConfig {
// //             data_filter: "hello/world".to_owned(),
// //             wildcard_filters: vec![],
// //             instant_ack: true,
// //             max_segment_size: 10 * 1024,
// //             max_segment_count: 10 * 1024,
// //             max_connections: 10,
// //         };

// //         let mut router = Router::new(0, config);
// //         let link = router.link();
// //         let handle = thread::spawn(move || {
// //             (0..count)
// //                 .map(|i| Link::new(&format!("link-{}", i), link.clone()).unwrap())
// //                 .collect()
// //         });

// //         router.run(count).unwrap();
// //         let links = handle.join().unwrap();
// //         (router, links)
// //     }

// //     /// Creates a connection
// //     fn connection(client_id: &str, clean: bool) -> (Connection, Receiver<Notification>) {
// //         let (tx, rx) = flume::bounded(10);
// //         let connection = Connection::new(client_id, clean, tx, Arc::new(AtomicBool::new(false)));
// //         (connection, rx)
// //     }

// //     /// Raw publish message
// //     fn publish(topic: &str) -> BytesMut {
// //         let mut publish = Publish::new(topic, QoS::AtLeastOnce, vec![1, 2, 3]);
// //         publish.pkid = 1;

// //         let mut o = BytesMut::new();
// //         publish.write(&mut o).unwrap();
// //         o
// //     }

// //     /// Raw publish message
// //     fn subscribe(topic: &str) -> BytesMut {
// //         let mut subscribe = Subscribe::new(topic, QoS::AtLeastOnce);
// //         subscribe.pkid = 1;

// //         let mut o = BytesMut::new();
// //         subscribe.write(&mut o).unwrap();
// //         o
// //     }

// //     /// When there is new data on a subscription's commitlog, it's
// //     /// consumer should start triggering data requests to pull data
// //     /// from commitlog
// //     #[test]
// //     fn new_connection_data_triggers_acks_of_self_and_data_of_consumer() {
// //         let (mut router, mut links) = router(2);
// //         let (mut l1tx, _l1rx) = links.pop_front().unwrap();
// //         let (mut l2tx, l2rx) = links.pop_front().unwrap();

// //         l2tx.subscribe("hello/world").unwrap();
// //         l1tx.publish("hello/world", vec![1, 2, 3]).unwrap();
// //         let _ = router.run(1);

// //         // Suback
// //         match l2rx.recv().unwrap() {
// //             Notification::DeviceAcks(_) => {}
// //             v => panic!("{:?}", v),
// //         }

// //         // Consumer data
// //         match l2rx.recv().unwrap() {
// //             Notification::DeviceData { cursor, .. } => assert_eq!(cursor, (0, 1)),
// //             v => panic!("{:?}", v),
// //         }

// //         // No acks for qos0 publish
// //     }

// //     #[test]
// //     fn half_open_connections_are_handled_correctly() {
// //         let config = RouterConfig {
// //             data_filter: "hello/world".to_owned(),
// //             wildcard_filters: vec![],
// //             instant_ack: true,
// //             max_segment_size: 10 * 1024,
// //             max_segment_count: 10 * 1024,
// //             max_connections: 10,
// //         };

// //         let mut router = Router::new(0, config);
// //         let (c, rx) = connection("test", true);
// //         router.handle_new_connection(c);

// //         let id = *router.connection_map.get("test").unwrap();
// //         router.handle_device_payload(id, subscribe("hello/data"));
// //         router.handle_device_payload(id, publish("hello/data"));

// //         let trackers = router.trackers.get(id).unwrap();
// //         assert!(trackers.get_data_requests().len() > 0);
// //         dbg!(trackers);

// //         // A new connection with same client id
// //         let (c, rx) = connection("test", true);
// //         router.handle_new_connection(c);

// //         let id = *router.connection_map.get("test").unwrap();
// //         let trackers = router.trackers.get(id).unwrap();
// //         dbg!(trackers);
// //     }
// // }
