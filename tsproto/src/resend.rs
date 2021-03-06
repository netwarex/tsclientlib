use std::cell::RefCell;
use std::cmp::{Ord, Ordering};
use std::collections::{binary_heap, BinaryHeap};
use std::convert::From;
use std::mem;
use std::ops::{Deref, DerefMut};
use std::rc::{Rc, Weak};
use std::time::Instant;

use chrono::{DateTime, Duration, Utc};
use futures::{self, Future, Sink};
use futures::task::{self, Task};
use slog::Logger;
use tokio_core::reactor::Timeout;

use Error;
use connection::Connection;
use connectionmanager::{ConnectionManager, Resender, ResenderEvent};
use handler_data::Data;
use packets::*;

/// A record of a packet that can be resent.
#[derive(Clone, Debug)]
struct SendRecord {
    /// When this packet was sent.
    pub sent: DateTime<Utc>,
    /// The last time when the packet was sent.
    pub last: DateTime<Utc>,
    /// How often the packet was already resent.
    pub tries: usize,
    pub p_type: PacketType,
    pub p_id: u16,
    /// The packet of this record.
    pub packet: UdpPacket,
}

impl PartialEq for SendRecord {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}
impl Eq for SendRecord {}

impl PartialOrd for SendRecord {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(&other))
    }
}

impl Ord for SendRecord {
    fn cmp(&self, other: &Self) -> Ordering {
        // If the packet was not already sent, it is more important
        if self.tries == 0 {
            if other.tries == 0 {
                self.p_id.cmp(&other.p_id).reverse()
            } else {
                Ordering::Greater
            }
        } else if other.tries == 0 {
            Ordering::Less
        } else {
            // The smallest time is the most important time
            match self.last.cmp(&other.last).reverse() {
                // Else, the lower packet id is more important
                Ordering::Equal => self.p_id.cmp(&other.p_id).reverse(),
                c => c,
            }
        }
    }
}

/// An implementation of a [`Resender`] that is provided by this library.
///
/// [`Resender`]:
pub struct DefaultResender {
    logger: Logger,

    state: ResendStates,
    config: ResendConfig,

    /// Smoothed Round Trip Time
    srtt: Duration,
    /// Deviation of the srtt.
    srtt_dev: Duration,

    /// The task of the sink, which is used to put new packets into the queue.
    ///
    /// This gets set, if the queue is full and the task should be notified,
    /// when an element is removed from the queue.
    resender_task: Vec<Task>,

    /// The task of the [`DefaultResenderFuture`]
    ///
    /// It should be notified when a new packet is inserted into the queue or
    /// the connection gets dropped.
    resender_future_task: Option<Task>,
}

impl DefaultResender {
    pub fn new(config: ResendConfig, logger: Logger) -> Self {
        let srtt = config.srtt;
        let srtt_dev = config.srtt_dev;
        Self {
            logger,
            state: ResendStates::Connecting {
                start_time: Utc::now(),
                to_send: Default::default(),
            },
            config,
            srtt,
            srtt_dev,

            resender_task: Vec::new(),
            resender_future_task: None,
        }
    }

    /// Add another duration to the stored smoothed rtt.
    pub fn update_srtt(&mut self, rtt: Duration) {
        let diff = if rtt > self.srtt {
            rtt - self.srtt
        } else {
            self.srtt - rtt
        };
        self.srtt_dev = self.srtt_dev * 3 / 4 + diff / 4;
        self.srtt = self.srtt * 7 / 8 + rtt / 8;
    }

    /// Replaces the current state by a new state and return the old state.
    fn set_state(&mut self, state: ResendStates) -> ResendStates {
        info!(self.logger, "Changed state"; "old" => self.state.get_name(),
            "new" => state.get_name());
        let old = mem::replace(&mut self.state, state);

        // Notify the future
        if let Some(ref task) = self.resender_future_task {
            task.notify();
        }
        old
    }
}

impl Drop for DefaultResender {
    fn drop(&mut self) {
        // Notify the future if the connection gets dropped.
        if let Some(ref task) = self.resender_future_task {
            task.notify();
        }
    }
}

impl Resender for DefaultResender {
    fn ack_packet(&mut self, p_type: PacketType, p_id: u16) {
        let rec = match self.state {
            ResendStates::Stalling { to_send: ref mut v, .. } |
            ResendStates::Dead     { to_send: ref mut v, .. } => {
                if let Some(i) = v.iter().position(|rec|
                    rec.p_type == p_type && rec.p_id == p_id) {
                    Some(v.remove(i))
                } else {
                    None
                }
            }
            ResendStates::Connecting    { ref mut to_send, .. } |
            ResendStates::Normal        { ref mut to_send     } |
            ResendStates::Disconnecting { ref mut to_send, .. } => {
                if let Some(is_first) = to_send.peek().map(|rec|
                    rec.p_type == p_type && rec.p_id == p_id) {
                    if is_first {
                        // Optimized to remove the first element
                        to_send.pop()
                    } else {
                        // Convert to vector to remove the element
                        let tmp = mem::replace(to_send, BinaryHeap::new());
                        let mut v = tmp.into_vec();
                        let mut rec = None;
                        if let Some(i) = v.iter().position(|rec|
                            rec.p_type == p_type && rec.p_id == p_id) {
                            rec = Some(v.remove(i));
                        }
                        mem::replace(to_send, v.into());
                        rec
                    }
                } else {
                    // Do nothing if the heap is empty
                    None
                }
            }
        };

        if let Some(rec) = rec {
            // Update srtt only if the packet was not resent
            if rec.tries == 1 {
                let now = Utc::now();
                let diff = now.naive_utc().signed_duration_since(
                    rec.sent.naive_utc());
                self.update_srtt(diff);
            }
        }

        // Switch to Normal mode if we are currently in stalling mode and
        // received an ack packet.
        let next_state = match self.state {
            ResendStates::Stalling { ref mut to_send, .. } => {
                for rec in to_send.iter_mut() {
                    // Reset tries
                    rec.tries = 0;
                }
                let to_send = mem::replace(to_send, Vec::new()).into();

                // Reset srtt, this will reset to stalling mode after 3 packets
                // are lost again.
                self.srtt = self.config.normal_timeout / 4;

                Some(ResendStates::Normal {
                    to_send,
                })
            }
            _ => None,
        };
        if let Some(next_state) = next_state {
            self.set_state(next_state);
            // Notify the resender future that the mode changed
            if let Some(ref task) = self.resender_future_task {
                task.notify();
            }
        }

        // Notify, that a packet was removed from the queue
        for t in self.resender_task.drain(..) {
            t.notify()
        }
    }

    fn send_voice_packets(&self, _: PacketType) -> bool {
        match self.state {
            ResendStates::Connecting    { .. } |
            ResendStates::Stalling      { .. } |
            ResendStates::Dead          { .. } |
            ResendStates::Disconnecting { .. } => false,
            ResendStates::Normal        { .. } => true,
        }
    }

    fn is_empty(&self) -> bool {
        match self.state {
            ResendStates::Connecting    { ref to_send, .. } |
            ResendStates::Disconnecting { ref to_send, .. } |
            ResendStates::Normal        { ref to_send, .. } =>
                to_send.is_empty(),
            ResendStates::Stalling      { ref to_send, .. } |
            ResendStates::Dead          { ref to_send, .. } =>
                to_send.is_empty(),
        }
    }

    fn handle_event(&mut self, event: ResenderEvent) {
        let next_state = match event {
            ResenderEvent::Connecting |
            ResenderEvent::Disconnecting => {
                // Switch to connecting or disconnecting state
                let now = Utc::now();
                let to_send = match self.state {
                    ResendStates::Stalling { ref mut to_send, .. } |
                    ResendStates::Dead     { ref mut to_send, .. } => {
                        // Sort by packet id
                        let mut v = mem::replace(to_send, Vec::new());
                        let res = v.drain(..)
                            .map(|mut rec| {
                                rec.tries = 0;
                                rec
                            }).collect();
                        res
                    }
                    ResendStates::Connecting    { ref mut to_send, .. } |
                    ResendStates::Normal        { ref mut to_send     } |
                    ResendStates::Disconnecting { ref mut to_send, .. } => {
                        let mut to_send = mem::replace(to_send,
                            BinaryHeap::new()).into_vec();
                        for rec in to_send.iter_mut() {
                            rec.tries = 0;
                        }

                        to_send.into()
                    }
                };

                if event == ResenderEvent::Connecting {
                    ResendStates::Connecting {
                        to_send,
                        start_time: now,
                    }
                } else {
                    ResendStates::Disconnecting {
                        to_send,
                        start_time: now,
                    }
                }
            }
            ResenderEvent::Connected =>
                // Switch to normal state
                match self.state {
                    ResendStates::Stalling { ref mut to_send, .. } |
                    ResendStates::Dead     { ref mut to_send, .. } => {
                        let to_send = mem::replace(to_send, Vec::new());
                        ResendStates::Normal { to_send: to_send.into() }
                    }
                    ResendStates::Connecting    { ref mut to_send, .. } |
                    ResendStates::Normal        { ref mut to_send     } |
                    ResendStates::Disconnecting { ref mut to_send, .. } => {
                        let to_send = mem::replace(to_send, BinaryHeap::new());
                        ResendStates::Normal { to_send }
                    }
                }
        };

        self.set_state(next_state);
        // Notify the resender future that the mode changed
        if let Some(ref task) = self.resender_future_task {
            task.notify();
        }
    }

    fn udp_packet_received(&mut self, _: &UdpPacket) {
        // Restart sending packets if we got a new packet
        let next_state = match self.state {
            ResendStates::Dead     { ref mut to_send, .. } => {
                let to_send = mem::replace(to_send, Vec::new()).into();
                // Switch to Stalling if the connection was dead
                Some(ResendStates::Stalling {
                    to_send,
                    start_time: Utc::now(),
                })
            }
            // We will switch to Normal from stalling after we received an ack
            // again
            _ => None,
        };
        if let Some(next_state) = next_state {
            self.set_state(next_state);
            // Notify the resender future that the mode changed
            if let Some(ref task) = self.resender_future_task {
                task.notify();
            }
        }
    }
}

impl Sink for DefaultResender {
    type SinkItem = (PacketType, u16, UdpPacket);
    type SinkError = Error;

    fn start_send(&mut self, (p_type, p_id, packet): Self::SinkItem)
        -> futures::StartSend<Self::SinkItem, Self::SinkError> {
        let rec = SendRecord {
            sent: Utc::now(),
            last: Utc::now(),
            tries: 0,
            p_type,
            p_id,
            packet,
        };

        // Put the packet into the queue if there is space left
        // otherwise, put it into rec_res.
        let mut rec_res = None;
        match self.state {
            ResendStates::Connecting    { to_send: ref mut v, ref mut start_time } |
            ResendStates::Disconnecting { to_send: ref mut v, ref mut start_time } => {
                if v.len() >= self.config.max_send_queue_len {
                    rec_res = Some(rec);
                } else {
                    v.push(rec);
                    // Update start time
                    *start_time = Utc::now();
                }
            }
            ResendStates::Stalling      { to_send: ref mut v, .. } |
            ResendStates::Dead          { to_send: ref mut v, .. } => {
                if v.len() >= self.config.max_send_queue_len {
                    rec_res = Some(rec);
                } else {
                    v.push(rec);
                }
            }
            ResendStates::Normal { to_send: ref mut v } => {
                if v.len() >= self.config.max_send_queue_len {
                    rec_res = Some(rec);
                } else {
                    v.push(rec);
                }
            }
        }

        if let Some(rec) = rec_res {
            // Set the task, so we get woken up if a place in the queue gets
            // free.
            self.resender_task.push(task::current());
            Ok(futures::AsyncSink::NotReady((rec.p_type, rec.p_id, rec.packet)))
        } else {
            // Notify the resender future that a new packet is available
            if let Some(ref task) = self.resender_future_task {
                task.notify();
            }
            Ok(futures::AsyncSink::Ready)
        }
    }

    fn poll_complete(&mut self) -> futures::Poll<(), Self::SinkError> {
        Ok(futures::Async::Ready(()))
    }
}

/// State per connection
///
/// In `Vec`s, the first element is the element that should be sent first, new
/// packets are appended at the end.
enum ResendStates {
    /// Important for clients: The first packet is sent, but we got no response
    /// yet, so we don't know if the server exists.
    ///
    /// The `Vec` is unsorted in this case as there exists no real sorting.
    Connecting {
        to_send: BinaryHeap<SendRecord>,
        start_time: DateTime<Utc>,
    },
    /// Everything is clear, normal operation.
    Normal {
        to_send: BinaryHeap<SendRecord>,
    },
    /// No acks were received for a while, so only try to resend the next packet
    /// until the connection is stable again.
    ///
    /// No voice packets are sent in this mode.
    Stalling {
        to_send: Vec<SendRecord>,
        start_time: DateTime<Utc>,
    },
    /// Resending did not succeed for a longer time. Don't even try anymore.
    ///
    /// No voice packets are sent in this mode.
    Dead {
        to_send: Vec<SendRecord>,
        start_time: DateTime<Utc>,
    },
    /// Sent the packet to close the connection, but the acknowledgement was not
    /// yet received.
    Disconnecting {
        to_send: BinaryHeap<SendRecord>,
        start_time: DateTime<Utc>,
    },
}

impl ResendStates {
    /// Returns the next record which should be sent, if there is one.
    fn peek_mut_next_record(&mut self) -> Option<PeekMut<SendRecord>> {
        match *self {
            ResendStates::Stalling { ref mut to_send, .. } =>
                to_send.first_mut().map(|r| r.into()),
            ResendStates::Connecting    { ref mut to_send, .. } |
            ResendStates::Normal        { ref mut to_send     } |
            ResendStates::Disconnecting { ref mut to_send, .. } =>
                to_send.peek_mut().map(|r| r.into()),
            ResendStates::Dead { .. } => None,
        }
    }

    fn get_packet_interval(&self, config: &ResendConfig) -> Option<Duration> {
        match *self {
            ResendStates::Connecting { .. } => Some(config.connecting_interval),
            ResendStates::Normal     { .. } => None,
            ResendStates::Stalling   { .. } => Some(config.stalling_interval),
            ResendStates::Dead       { .. } => None,
            ResendStates::Disconnecting { .. } => Some(config.disconnect_interval),
        }
    }

    fn get_name(&self) -> &'static str {
        match *self {
            ResendStates::Connecting    { .. } => "Connecting",
            ResendStates::Normal        { .. } => "Normal",
            ResendStates::Stalling      { .. } => "Stalling",
            ResendStates::Dead          { .. } => "Dead",
            ResendStates::Disconnecting { .. } => "Disconnecting",
        }
    }
}

enum PeekMut<'a, T: Ord + 'a> {
    Ref(&'a mut T),
    Heap(binary_heap::PeekMut<'a, T>),
}

impl<'a, T: Ord + 'a> Deref for PeekMut<'a, T> {
    type Target = T;

    fn deref(&self) -> &T {
        match *self {
            PeekMut::Ref(ref r) => r,
            PeekMut::Heap(ref r) => r.deref(),
        }
    }
}

impl<'a, T: Ord + 'a> DerefMut for PeekMut<'a, T> {
    fn deref_mut(&mut self) -> &mut T {
        match *self {
            PeekMut::Ref(ref mut r) => r,
            PeekMut::Heap(ref mut r) => r.deref_mut(),
        }
    }
}

impl<'a, T: Ord + 'a> From<&'a mut T> for PeekMut<'a, T> {
    fn from(t: &'a mut T) -> PeekMut<'a, T> {
        PeekMut::Ref(t)
    }
}

impl<'a, T: Ord + 'a> From<binary_heap::PeekMut<'a, T>> for PeekMut<'a, T> {
    fn from(t: binary_heap::PeekMut<'a, T>) -> PeekMut<'a, T> {
        PeekMut::Heap(t)
    }
}

/// Configure the length of timeouts.
#[derive(Clone, Debug)]
pub struct ResendConfig {
    /// Interval to resend the first packet.
    pub connecting_interval: Duration,
    /// Timeout to give up sending the first packet and close the connection.
    pub connecting_timeout: Duration,
    /// Swith to [`Stalling`] when no awaited response was received after this
    /// duration (added to the current estimated response time).
    pub normal_timeout: Duration,
    /// Interval to resend the first packet in [`Stalling`] mode.
    pub stalling_interval: Duration,
    /// Switch to [`Dead`] when no awaited response was received after this
    /// duration.
    pub stalling_timeout: Duration,
    /// When in [`Dead`] state, close the connection after no packet is received
    /// for this duration.
    pub dead_timeout: Duration,
    /// When in [`Disconnecting`] state, close the connection after no packet is
    /// received for this duration.
    pub disconnect_timeout: Duration,
    /// Interval to resend the disconnect packet.
    pub disconnect_interval: Duration,

    /// Start value for the Smoothed Round Trip Time.
    pub srtt: Duration,
    /// Start value for the deviation of the srtt.
    pub srtt_dev: Duration,

    /// The maximum number of not acknowledged packets which are stored.
    pub max_send_queue_len: usize,
}

impl Default for ResendConfig {
    fn default() -> Self {
        ResendConfig {
            connecting_interval: Duration::seconds(1),
            connecting_timeout: Duration::seconds(5),
            normal_timeout: Duration::seconds(10),
            stalling_interval: Duration::seconds(5),
            stalling_timeout: Duration::seconds(30),
            dead_timeout: Duration::seconds(0),
            disconnect_timeout: Duration::seconds(5),
            disconnect_interval: Duration::seconds(1),

            srtt: Duration::milliseconds(2500),
            srtt_dev: Duration::milliseconds(0),

            max_send_queue_len: 50,
        }
    }
}

/// This future is running in parallel to the rest and is responsible for
/// sending all command packets.
pub struct ResendFuture<CM: ConnectionManager + 'static> {
    data: Weak<RefCell<Data<CM>>>,
    connection_key: CM::ConnectionsKey,
    connection: Weak<RefCell<Connection<CM>>>,
    sink: ::connection::UdpPackets<CM>,
    /// The future to wake us up when the next packet should be resent.
    timeout: Timeout,
    /// The future to wake us up when the current state times out.
    state_timeout: Timeout,
    /// If we are sending and should poll the sink.
    is_sending: bool,
}

impl<CM: ConnectionManager + 'static> ResendFuture<CM> {
    pub fn new(
        data: Rc<RefCell<Data<CM>>>,
        connection_key: CM::ConnectionsKey,
    ) -> Self {
        let (handle, connection) = {
            let data = data.borrow();
            (data.handle.clone(),
                data.connection_manager.get_connection(connection_key.clone())
                .unwrap())
        };
        Self {
            data: Rc::downgrade(&data),
            connection_key,
            connection: Rc::downgrade(&connection),
            sink: Connection::get_udp_packets(connection),
            timeout: Timeout::new(
                Duration::seconds(1).to_std().unwrap(),
                &handle,
            ).unwrap(),
            state_timeout: Timeout::new(
                Duration::seconds(1).to_std().unwrap(),
                &handle,
            ).unwrap(),
            is_sending: false,
        }
    }
}

impl<CM: ConnectionManager<Resend = DefaultResender> + 'static> Future for
    ResendFuture<CM> {
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> futures::Poll<Self::Item, Self::Error> {
        // Get connection
        let con = if let Some(con) = self.connection.upgrade() {
            con
        } else {
            // Quit if the connection does not exist anymore
            return Ok(futures::Async::Ready(()));
        };
        {
            // Set task
            let mut con = con.borrow_mut();
            con.resender.resender_future_task = Some(task::current());
        }

        if self.is_sending {
            if let futures::Async::Ready(()) = self.sink.poll_complete()? {
                self.is_sending = false;
            } else {
                return Ok(futures::Async::NotReady);
            }
        }

        let now = Utc::now();
        let now_naive = now.naive_utc();

        // Check if we are over time in the current state
        enum StateChange {
            Nothing,
            EndConnection,
            NewState(ResendStates),
        }

        let next_state = {
            let mut con = con.borrow_mut();
            let resender = &mut con.resender;
            match resender.state {
                ResendStates::Connecting { ref start_time, .. } =>
                    if now_naive.signed_duration_since(start_time.naive_utc())
                        >= resender.config
                            .connecting_timeout {
                        StateChange::EndConnection
                    } else {
                        // Schedule timeout
                        let dur = (*start_time + resender.config
                                .connecting_timeout)
                            .naive_utc()
                            .signed_duration_since(now_naive);
                        let next = Instant::now() + dur.to_std().unwrap();
                        self.state_timeout.reset(next);
                        if let futures::Async::Ready(()) =
                            self.state_timeout.poll()? {
                            task::current().notify();
                        }
                        StateChange::Nothing
                    }
                ResendStates::Normal { .. } => StateChange::Nothing,
                ResendStates::Stalling { ref mut to_send, ref start_time } =>
                    if now_naive.signed_duration_since(start_time.naive_utc())
                        >= resender.config.stalling_timeout {
                        StateChange::NewState(ResendStates::Dead {
                            to_send: mem::replace(to_send, Vec::new()),
                            start_time: Utc::now(),
                        })
                    } else {
                        // Schedule timeout
                        let dur = (*start_time + resender.config
                                .stalling_timeout)
                            .naive_utc()
                            .signed_duration_since(now_naive);
                        let next = Instant::now() + dur.to_std().unwrap();
                        self.state_timeout.reset(next);
                        if let futures::Async::Ready(()) =
                            self.state_timeout.poll()? {
                            task::current().notify();
                        }
                        StateChange::Nothing
                    }
                ResendStates::Dead { ref start_time, .. } =>
                    if now_naive.signed_duration_since(start_time.naive_utc())
                        >= resender.config.dead_timeout {
                        StateChange::EndConnection
                    } else {
                        // Schedule timeout
                        let dur = (*start_time + resender.config
                                .dead_timeout)
                            .naive_utc()
                            .signed_duration_since(now_naive);
                        let next = Instant::now() + dur.to_std().unwrap();
                        self.state_timeout.reset(next);
                        if let futures::Async::Ready(()) =
                            self.state_timeout.poll()? {
                            task::current().notify();
                        }
                        StateChange::Nothing
                    }
                ResendStates::Disconnecting { ref start_time, .. } =>
                    if now_naive.signed_duration_since(start_time.naive_utc())
                        >= resender.config.disconnect_timeout {
                        StateChange::EndConnection
                    } else {
                        // Schedule timeout
                        let dur = (*start_time + resender.config
                                .disconnect_timeout)
                            .naive_utc()
                            .signed_duration_since(now_naive);
                        let next = Instant::now() + dur.to_std().unwrap();
                        self.state_timeout.reset(next);
                        if let futures::Async::Ready(()) =
                            self.state_timeout.poll()? {
                            task::current().notify();
                        }
                        StateChange::Nothing
                    }
            }
        };

        if let StateChange::NewState(next_state) = next_state {
            let mut con = con.borrow_mut();
            con.resender.set_state(next_state);
            // Queue the next immideate update
            task::current().notify();
            return Ok(futures::Async::NotReady);
        } else if let StateChange::EndConnection = next_state {
            // End connection
            let data = self.data.upgrade().unwrap();
            let con = con.borrow();
            info!(con.logger, "Exiting connection because it is not responding";
                "current state" => con.resender.state.get_name());
            Data::remove_connection(data, self.connection_key.clone());
            return Ok(futures::Async::NotReady);
        }

        // Check if there are packets to send.
        // If there is no record, we will be notified by the sink.
        let mut switch_to_stalling = false;
        let packet_interval = {
            let con = &*con.borrow();
            con.resender.state
                .get_packet_interval(&con.resender.config)
        };

        // Retransmission timeout
        let rto = {
            let con = con.borrow();
            if let Some(interval) = packet_interval {
                interval
            } else {
                con.resender.srtt + con.resender.srtt_dev * 4
            }
        };
        let last_threshold = now - rto;

        while let Some(packet) = {
            let con = &mut *con.borrow_mut();
            let packet = if let Some(mut rec) = con.resender.state.peek_mut_next_record() {
                // Check if we should resend this packet or not
                if rec.tries != 0 && rec.last > last_threshold {
                    // Schedule next send
                    let dur = rec.last
                        .naive_utc()
                        .signed_duration_since(last_threshold.naive_utc());
                    let next = Instant::now() + dur.to_std().unwrap();
                    self.timeout.reset(next);
                    if let futures::Async::Ready(()) = self.timeout.poll()? {
                        task::current().notify();
                    }
                    return Ok(futures::Async::NotReady);
                }

                // Print packet for debugging
                //info!(con.logger, "Packet in send queue"; "id" => ?rec.p_id,
                //    "last" => ?rec.last);
                Some(rec.packet.clone())
            } else {
                //info!(con.logger, "No packet in send queue");
                None
            };
            packet
        } {
            // Try to send this packet
            if let futures::AsyncSink::NotReady(_) =
                self.sink.start_send(packet)?
            {
                // The sink should notify us if it is ready
                break;
            } else {
                // Successfully started sending the packet, now schedule the
                // next send time for this packet and enqueue it.
                if let futures::Async::Ready(()) = self.sink.poll_complete()? {
                    self.is_sending = false;
                } else {
                    self.is_sending = true;
                }

                let con = &mut *con.borrow_mut();
                let mut rec = con.resender.state.peek_mut_next_record().unwrap();

                // Double srtt on packet loss
                if rec.tries != 0 && con.resender.srtt
                    < con.resender.config.normal_timeout {
                    con.resender.srtt = con.resender.srtt * 2;
                }

                let is_normal_state = if let ResendStates::Normal { .. } =
                    con.resender.state {
                    true
                } else {
                    false
                };

                let next = now + rto;
                let dur = next.naive_utc().signed_duration_since(
                    now.naive_utc());

                if is_normal_state && dur > con.resender.config.normal_timeout {
                    warn!(con.logger, "Max resend timeout exceeded";
                          "p_id" => rec.p_id);
                    // Switch connection to stalling state
                    switch_to_stalling = true;
                    break;
                }

                // Update record
                rec.last = now;
                rec.tries += 1;

                if rec.tries != 1 {
                    let to_s = if con.is_client { "S" } else { "C" };
                    warn!(con.logger, "Resend";
                        "p_id" => rec.p_id,
                        "tries" => rec.tries,
                        "last" => %rec.last,
                        "to" => to_s,
                    );
                }
            }
        }

        if switch_to_stalling {
            let mut con = con.borrow_mut();
            let mut to_send = if let ResendStates::Normal { ref mut to_send } =
                con.resender.state {
                mem::replace(to_send, BinaryHeap::new()).into_vec()
            } else {
                unreachable!("Connection was not in normal state");
            };
            to_send.sort_by(|a, b| a.p_id.cmp(&b.p_id));

            con.resender.set_state(ResendStates::Stalling {
                to_send,
                start_time: now,
            });
            task::current().notify();
        }

        Ok(futures::Async::NotReady)
    }
}
