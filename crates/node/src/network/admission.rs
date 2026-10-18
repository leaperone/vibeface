use std::{
    collections::HashMap,
    fmt,
    net::IpAddr,
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant},
};
use tokio::sync::oneshot;

pub const MAX_SOCKETS: usize = 128;
pub const MAX_INITIAL: usize = 16;
pub const MAX_ESTABLISHED: usize = MAX_SOCKETS - MAX_INITIAL;
pub const MAX_CIRCUITS: usize = 32;
const MAX_SOURCE_INITIAL: usize = 4;
const MAX_SOURCE_ESTABLISHED: usize = 16;
const MAX_SOURCE_CIRCUITS: usize = 4;
const MAX_TARGET_CIRCUITS: usize = 4;
const MAX_SOURCES: usize = 2048;
const GLOBAL_REQUESTS: usize = 600;
const SOURCE_REQUESTS: usize = 120;
const REQUEST_WINDOW: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SocketClass {
    Initial,
    Control,
    Reservation,
    Data,
}

impl SocketClass {
    fn short(self) -> bool {
        matches!(self, Self::Initial | Self::Control)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Denied {
    GlobalSocket,
    InitialSocket,
    EstablishedSocket,
    SourceInitialSocket,
    SourceEstablishedSocket,
    SourceTable,
    SourceRate,
    GlobalRate,
    GlobalCircuit,
    SourceCircuit,
    TargetCircuit,
    HandoffUnavailable,
    HandoffPending,
}

impl Denied {
    pub fn code(self) -> &'static str {
        match self {
            Self::GlobalSocket => "global_socket",
            Self::InitialSocket => "initial_socket",
            Self::EstablishedSocket => "established_socket",
            Self::SourceInitialSocket => "source_initial_socket",
            Self::SourceEstablishedSocket => "source_established_socket",
            Self::SourceTable => "source_table",
            Self::SourceRate => "source_rate",
            Self::GlobalRate => "global_rate",
            Self::GlobalCircuit => "global_circuit",
            Self::SourceCircuit => "source_circuit",
            Self::TargetCircuit => "target_circuit",
            Self::HandoffUnavailable => "handoff_unavailable",
            Self::HandoffPending => "handoff_pending",
        }
    }

    pub fn rate(self) -> bool {
        matches!(self, Self::SourceRate | Self::GlobalRate)
    }
}

impl fmt::Display for Denied {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}
impl std::error::Error for Denied {}

#[derive(Debug, Default)]
struct RequestRate {
    start: Option<Instant>,
    count: usize,
}

impl RequestRate {
    fn refresh(&mut self, now: Instant) {
        if self
            .start
            .is_none_or(|start| now.duration_since(start) >= REQUEST_WINDOW)
        {
            self.start = Some(now);
            self.count = 0;
        }
    }

    fn available(&mut self, now: Instant, limit: usize) -> bool {
        self.refresh(now);
        self.count < limit
    }

    fn record(&mut self, now: Instant) {
        self.refresh(now);
        self.count += 1;
    }

    fn active(&self, now: Instant) -> bool {
        self.count > 0
            && self
                .start
                .is_some_and(|start| now.duration_since(start) < REQUEST_WINDOW)
    }
}

#[derive(Debug, Default)]
struct Source {
    short: usize,
    long: usize,
    held_long: usize,
    circuits: usize,
    requests: RequestRate,
}

impl Source {
    fn active(&self, now: Instant) -> bool {
        self.short > 0
            || self.long > 0
            || self.held_long > 0
            || self.circuits > 0
            || self.requests.active(now)
    }
}

#[derive(Debug, Default)]
struct State {
    total: usize,
    short: usize,
    long: usize,
    held_long: usize,
    circuits: usize,
    requests: RequestRate,
    sources: HashMap<IpAddr, Source>,
    targets: HashMap<String, usize>,
    next_slot: u64,
    slots: HashMap<u64, Slot>,
    promotions: HashMap<u64, Promotion>,
}

#[derive(Clone, Copy, Debug)]
struct Slot {
    source: Option<IpAddr>,
    class: SocketClass,
}

#[derive(Debug)]
struct Promotion {
    old_slot: u64,
    source: Option<IpAddr>,
    source_held: bool,
    old_closed: bool,
    closed: Option<oneshot::Sender<()>>,
}

impl State {
    fn check_class(&self, source: Option<IpAddr>, class: SocketClass) -> Result<(), Denied> {
        if let Some(source) = source {
            if let Some(budget) = self.sources.get(&source) {
                if class.short() && budget.short >= MAX_SOURCE_INITIAL {
                    return Err(Denied::SourceInitialSocket);
                }
                if !class.short() && budget.long + budget.held_long >= MAX_SOURCE_ESTABLISHED {
                    return Err(Denied::SourceEstablishedSocket);
                }
            } else if self.sources.len() >= MAX_SOURCES {
                return Err(Denied::SourceTable);
            }
        }
        if class.short() && self.short >= MAX_INITIAL {
            return Err(Denied::InitialSocket);
        }
        if !class.short() && self.long + self.held_long >= MAX_ESTABLISHED {
            return Err(Denied::EstablishedSocket);
        }
        Ok(())
    }

    fn add_class(&mut self, source: Option<IpAddr>, class: SocketClass) {
        if class.short() {
            self.short += 1;
        } else {
            self.long += 1;
        }
        if let Some(source) = source {
            let budget = self.sources.entry(source).or_default();
            if class.short() {
                budget.short += 1;
            } else {
                budget.long += 1;
            }
        }
    }

    fn remove_class(&mut self, source: Option<IpAddr>, class: SocketClass) {
        if class.short() {
            self.short -= 1;
        } else {
            self.long -= 1;
        }
        if let Some(source) = source {
            let budget = self
                .sources
                .get_mut(&source)
                .expect("admitted source disappeared");
            if class.short() {
                budget.short -= 1;
            } else {
                budget.long -= 1;
            }
        }
    }

    fn prune(&mut self, now: Instant) {
        self.sources.retain(|_, source| source.active(now));
    }

    fn slot(&mut self) -> u64 {
        loop {
            self.next_slot = self.next_slot.wrapping_add(1);
            if !self.slots.contains_key(&self.next_slot)
                && !self
                    .promotions
                    .values()
                    .any(|promotion| promotion.old_slot == self.next_slot)
            {
                return self.next_slot;
            }
        }
    }

    fn remove_promotion(&mut self, slot: u64) {
        if let Some(promotion) = self.promotions.remove(&slot) {
            if promotion.old_closed {
                self.held_long -= 1;
            }
            if promotion.source_held {
                self.sources
                    .get_mut(&promotion.source.expect("held source missing"))
                    .expect("held source disappeared")
                    .held_long -= 1;
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct Admission {
    state: Mutex<State>,
}

impl Admission {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    pub fn socket(
        self: &Arc<Self>,
        source: Option<IpAddr>,
        class: SocketClass,
    ) -> Result<SocketAdmission, Denied> {
        let now = Instant::now();
        let mut state = self.lock();
        state.prune(now);
        if state.total >= MAX_SOCKETS {
            return Err(Denied::GlobalSocket);
        }
        state.check_class(source, class)?;
        if let Some(source) = source {
            if state
                .sources
                .get_mut(&source)
                .is_some_and(|budget| !budget.requests.available(now, SOURCE_REQUESTS))
            {
                return Err(Denied::SourceRate);
            }
            if !state.requests.available(now, GLOBAL_REQUESTS) {
                return Err(Denied::GlobalRate);
            }
            state.requests.record(now);
            state
                .sources
                .entry(source)
                .or_default()
                .requests
                .record(now);
        }
        state.total += 1;
        state.add_class(source, class);
        let id = state.slot();
        state.slots.insert(id, Slot { source, class });
        Ok(SocketAdmission {
            admission: self.clone(),
            source,
            class,
            id,
        })
    }

    pub fn circuit(
        self: &Arc<Self>,
        source: IpAddr,
        target: &str,
    ) -> Result<CircuitAdmission, Denied> {
        let mut state = self.lock();
        state.prune(Instant::now());
        if let Some(budget) = state.sources.get(&source) {
            if budget.circuits >= MAX_SOURCE_CIRCUITS {
                return Err(Denied::SourceCircuit);
            }
        } else if state.sources.len() >= MAX_SOURCES {
            return Err(Denied::SourceTable);
        }
        if state
            .targets
            .get(target)
            .is_some_and(|count| *count >= MAX_TARGET_CIRCUITS)
        {
            return Err(Denied::TargetCircuit);
        }
        if state.circuits >= MAX_CIRCUITS {
            return Err(Denied::GlobalCircuit);
        }
        state.circuits += 1;
        state.sources.entry(source).or_default().circuits += 1;
        *state.targets.entry(target.to_owned()).or_default() += 1;
        Ok(CircuitAdmission {
            admission: self.clone(),
            source,
            target: target.to_owned(),
        })
    }

    #[cfg(test)]
    pub fn snapshot(&self) -> Snapshot {
        let state = self.lock();
        Snapshot {
            total: state.total,
            short: state.short,
            long: state.long,
            held_long: state.held_long,
            promotions: state.promotions.len(),
            slots: state.slots.len(),
            circuits: state.circuits,
            inbound_requests: state.requests.count,
            sources: state
                .sources
                .iter()
                .map(|(ip, source)| {
                    (
                        *ip,
                        SourceSnapshot {
                            short: source.short,
                            long: source.long,
                            held_long: source.held_long,
                            circuits: source.circuits,
                            requests: source.requests.count,
                        },
                    )
                })
                .collect(),
            targets: state.targets.clone(),
        }
    }
}

#[derive(Debug)]
pub struct SocketAdmission {
    admission: Arc<Admission>,
    source: Option<IpAddr>,
    class: SocketClass,
    id: u64,
}

impl SocketAdmission {
    pub fn source(&self) -> Option<IpAddr> {
        self.source
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn promote(&mut self, class: SocketClass) -> Result<(), Denied> {
        let mut state = self.admission.lock();
        if state.promotions.contains_key(&self.id)
            || (class != self.class
                && state
                    .promotions
                    .values()
                    .any(|promotion| promotion.old_slot == self.id))
        {
            return Err(Denied::HandoffPending);
        }
        if self.class.short() != class.short() {
            state.check_class(self.source, class)?;
            state.remove_class(self.source, self.class);
            state.add_class(self.source, class);
        }
        state
            .slots
            .get_mut(&self.id)
            .expect("admitted slot disappeared")
            .class = class;
        self.class = class;
        Ok(())
    }

    pub fn handoff(&self, old_slot: u64) -> Result<PendingPromotion, Denied> {
        let mut state = self.admission.lock();
        if self.class != SocketClass::Initial || old_slot == self.id {
            return Err(Denied::HandoffUnavailable);
        }
        if state.promotions.contains_key(&self.id)
            || state
                .promotions
                .values()
                .any(|promotion| promotion.old_slot == old_slot)
        {
            return Err(Denied::HandoffPending);
        }
        let old = *state
            .slots
            .get(&old_slot)
            .filter(|slot| slot.class == SocketClass::Reservation)
            .ok_or(Denied::HandoffUnavailable)?;
        if state.long + state.held_long > MAX_ESTABLISHED {
            return Err(Denied::EstablishedSocket);
        }
        if let Some(source) = self.source {
            let budget = state.sources.get(&source).expect("new source disappeared");
            let credit = usize::from(old.source == self.source);
            if budget.long + budget.held_long - credit >= MAX_SOURCE_ESTABLISHED {
                return Err(Denied::SourceEstablishedSocket);
            }
        }
        let source_held = self.source.is_some() && self.source != old.source;
        if source_held {
            state
                .sources
                .get_mut(&self.source.unwrap())
                .unwrap()
                .held_long += 1;
        }
        let (closed, receive) = oneshot::channel();
        state.promotions.insert(
            self.id,
            Promotion {
                old_slot,
                source: self.source,
                source_held,
                old_closed: false,
                closed: Some(closed),
            },
        );
        Ok(PendingPromotion {
            admission: self.admission.clone(),
            slot: self.id,
            active: true,
            closed: receive,
        })
    }

    pub fn finish_handoff(&mut self, ticket: &mut PendingPromotion) -> Result<(), Denied> {
        if !ticket.active
            || ticket.slot != self.id
            || !Arc::ptr_eq(&ticket.admission, &self.admission)
        {
            return Err(Denied::HandoffUnavailable);
        }
        let mut state = self.admission.lock();
        let promotion = state
            .promotions
            .get(&self.id)
            .ok_or(Denied::HandoffUnavailable)?;
        if !promotion.old_closed {
            return Err(Denied::HandoffPending);
        }
        if state.long + state.held_long > MAX_ESTABLISHED {
            return Err(Denied::EstablishedSocket);
        }
        if let Some(source) = self.source {
            let budget = state
                .sources
                .get(&source)
                .expect("handoff source disappeared");
            if budget.long + budget.held_long > MAX_SOURCE_ESTABLISHED {
                return Err(Denied::SourceEstablishedSocket);
            }
        }
        state.remove_promotion(self.id);
        state.remove_class(self.source, self.class);
        state.add_class(self.source, SocketClass::Reservation);
        state
            .slots
            .get_mut(&self.id)
            .expect("handoff slot disappeared")
            .class = SocketClass::Reservation;
        self.class = SocketClass::Reservation;
        ticket.active = false;
        Ok(())
    }
}

impl Drop for SocketAdmission {
    fn drop(&mut self) {
        let notify = {
            let mut state = self.admission.lock();
            state.remove_promotion(self.id);
            state
                .slots
                .remove(&self.id)
                .expect("admitted slot disappeared");
            state.total -= 1;
            state.remove_class(self.source, self.class);
            let pending = state
                .promotions
                .iter()
                .find_map(|(slot, promotion)| (promotion.old_slot == self.id).then_some(*slot));
            let notify = if let Some(slot) = pending {
                let promotion = state.promotions.get_mut(&slot).unwrap();
                promotion.old_closed = true;
                let new_source = promotion.source;
                let add_source = new_source.is_some() && !promotion.source_held;
                if add_source {
                    promotion.source_held = true;
                }
                let notify = promotion.closed.take();
                state.held_long += 1;
                if add_source {
                    state
                        .sources
                        .get_mut(&new_source.unwrap())
                        .unwrap()
                        .held_long += 1;
                }
                notify
            } else {
                None
            };
            state.prune(Instant::now());
            notify
        };
        if let Some(notify) = notify {
            let _ = notify.send(());
        }
    }
}

#[derive(Debug)]
pub struct PendingPromotion {
    admission: Arc<Admission>,
    slot: u64,
    active: bool,
    closed: oneshot::Receiver<()>,
}

impl PendingPromotion {
    pub async fn closed(&mut self) -> Result<(), Denied> {
        (&mut self.closed)
            .await
            .map_err(|_| Denied::HandoffUnavailable)
    }
}

impl Drop for PendingPromotion {
    fn drop(&mut self) {
        if self.active {
            let mut state = self.admission.lock();
            state.remove_promotion(self.slot);
            state.prune(Instant::now());
        }
    }
}

#[derive(Debug)]
pub struct CircuitAdmission {
    admission: Arc<Admission>,
    source: IpAddr,
    target: String,
}

impl Drop for CircuitAdmission {
    fn drop(&mut self) {
        let mut state = self.admission.lock();
        state.circuits -= 1;
        state
            .sources
            .get_mut(&self.source)
            .expect("circuit source disappeared")
            .circuits -= 1;
        let count = state
            .targets
            .get_mut(&self.target)
            .expect("circuit target disappeared");
        *count -= 1;
        if *count == 0 {
            state.targets.remove(&self.target);
        }
        state.prune(Instant::now());
    }
}

#[cfg(test)]
#[derive(Debug, PartialEq, Eq)]
pub struct SourceSnapshot {
    pub short: usize,
    pub long: usize,
    pub held_long: usize,
    pub circuits: usize,
    pub requests: usize,
}

#[cfg(test)]
#[derive(Debug)]
pub struct Snapshot {
    pub total: usize,
    pub short: usize,
    pub long: usize,
    pub held_long: usize,
    pub promotions: usize,
    pub slots: usize,
    pub circuits: usize,
    pub inbound_requests: usize,
    pub sources: HashMap<IpAddr, SourceSnapshot>,
    pub targets: HashMap<String, usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(index: u8) -> IpAddr {
        IpAddr::from([127, 0, 0, index])
    }

    #[tokio::test]
    async fn full_source_and_global_handoff_waits_for_drop_and_protects_the_promised_seat() {
        let admission = Arc::new(Admission::default());
        let mut old = admission
            .socket(Some(ip(1)), SocketClass::Reservation)
            .unwrap();
        let mut held = Vec::new();
        for _ in 0..15 {
            held.push(admission.socket(Some(ip(1)), SocketClass::Data).unwrap());
        }
        for _ in 16..MAX_ESTABLISHED {
            held.push(admission.socket(None, SocketClass::Data).unwrap());
        }
        let mut next = admission.socket(Some(ip(1)), SocketClass::Initial).unwrap();
        let mut ticket = next.handoff(old.id()).unwrap();
        assert_eq!(
            old.promote(SocketClass::Control),
            Err(Denied::HandoffPending)
        );
        assert_eq!(
            next.finish_handoff(&mut ticket),
            Err(Denied::HandoffPending)
        );
        assert_eq!(
            (
                admission.snapshot().long,
                admission.snapshot().held_long,
                admission.snapshot().short
            ),
            (112, 0, 1)
        );
        assert!(matches!(
            ticket.closed.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        drop(old);
        assert_eq!(
            (admission.snapshot().long, admission.snapshot().held_long),
            (111, 1)
        );
        assert_eq!(admission.snapshot().sources[&ip(1)].held_long, 1);
        assert_eq!(
            admission.socket(None, SocketClass::Data).unwrap_err(),
            Denied::EstablishedSocket
        );
        assert_eq!(
            admission
                .socket(Some(ip(1)), SocketClass::Data)
                .unwrap_err(),
            Denied::SourceEstablishedSocket
        );
        ticket.closed().await.unwrap();
        next.finish_handoff(&mut ticket).unwrap();
        assert_eq!(
            (
                admission.snapshot().long,
                admission.snapshot().held_long,
                admission.snapshot().short,
                admission.snapshot().promotions
            ),
            (112, 0, 0, 0)
        );
        drop(next);
        drop(held);
        assert_eq!(
            (admission.snapshot().total, admission.snapshot().slots),
            (0, 0)
        );
    }

    #[test]
    fn cross_source_full_rejects_before_claiming_or_releasing_the_old_reservation() {
        let admission = Arc::new(Admission::default());
        let old = admission
            .socket(Some(ip(1)), SocketClass::Reservation)
            .unwrap();
        let held: Vec<_> = (0..16)
            .map(|_| admission.socket(Some(ip(2)), SocketClass::Data).unwrap())
            .collect();
        let next = admission.socket(Some(ip(2)), SocketClass::Initial).unwrap();
        assert_eq!(
            next.handoff(old.id()).unwrap_err(),
            Denied::SourceEstablishedSocket
        );
        assert_eq!(admission.snapshot().promotions, 0);
        assert!(admission.lock().slots.contains_key(&old.id()));
        assert_eq!(admission.snapshot().sources[&ip(1)].long, 1);
        drop(next);
        drop(old);
        drop(held);
        assert_eq!(admission.snapshot().total, 0);
    }

    #[tokio::test]
    async fn cross_source_handoff_reserves_the_new_source_before_old_drop() {
        let admission = Arc::new(Admission::default());
        let old = admission
            .socket(Some(ip(1)), SocketClass::Reservation)
            .unwrap();
        let held: Vec<_> = (0..15)
            .map(|_| admission.socket(Some(ip(2)), SocketClass::Data).unwrap())
            .collect();
        let mut next = admission.socket(Some(ip(2)), SocketClass::Initial).unwrap();
        let mut ticket = next.handoff(old.id()).unwrap();
        assert_eq!(
            (admission.snapshot().long, admission.snapshot().held_long),
            (16, 0)
        );
        assert_eq!(admission.snapshot().sources[&ip(2)].held_long, 1);
        assert_eq!(
            admission
                .socket(Some(ip(2)), SocketClass::Data)
                .unwrap_err(),
            Denied::SourceEstablishedSocket
        );
        drop(admission.socket(Some(ip(3)), SocketClass::Data).unwrap());
        drop(old);
        ticket.closed().await.unwrap();
        next.finish_handoff(&mut ticket).unwrap();
        assert_eq!(
            (
                admission.snapshot().sources[&ip(2)].long,
                admission.snapshot().sources[&ip(2)].held_long
            ),
            (16, 0)
        );
        drop(next);
        drop(held);
        assert_eq!(admission.snapshot().total, 0);
    }

    #[test]
    fn pending_second_generation_cannot_back_or_duplicate_a_third_generation_promise() {
        let admission = Arc::new(Admission::default());
        let old = admission.socket(None, SocketClass::Reservation).unwrap();
        let next = admission.socket(None, SocketClass::Initial).unwrap();
        let third = admission.socket(None, SocketClass::Initial).unwrap();
        let ticket = next.handoff(old.id()).unwrap();
        assert_eq!(third.handoff(old.id()).unwrap_err(), Denied::HandoffPending);
        assert_eq!(
            third.handoff(next.id()).unwrap_err(),
            Denied::HandoffUnavailable
        );
        assert_eq!(admission.snapshot().promotions, 1);
        drop(ticket);
        assert_eq!(admission.snapshot().promotions, 0);
        drop(old);
        assert_eq!(admission.snapshot().held_long, 0);
        drop(next);
        drop(third);
        assert_eq!(admission.snapshot().slots, 0);
    }

    #[test]
    fn cancelling_ticket_before_or_after_old_drop_withdraws_each_promise_once() {
        for close_old_first in [false, true] {
            let admission = Arc::new(Admission::default());
            let mut old = Some(
                admission
                    .socket(Some(ip(1)), SocketClass::Reservation)
                    .unwrap(),
            );
            let next = admission.socket(Some(ip(2)), SocketClass::Initial).unwrap();
            let ticket = next.handoff(old.as_ref().unwrap().id()).unwrap();
            if close_old_first {
                drop(old.take());
            }
            drop(ticket);
            assert_eq!(
                (
                    admission.snapshot().held_long,
                    admission.snapshot().promotions,
                    admission.snapshot().sources[&ip(2)].held_long
                ),
                (0, 0, 0)
            );
            drop(old);
            drop(next);
            assert_eq!(
                (
                    admission.snapshot().held_long,
                    admission.snapshot().total,
                    admission.snapshot().slots
                ),
                (0, 0, 0)
            );
        }
    }

    #[tokio::test]
    async fn dropping_the_new_socket_revokes_its_ticket_without_closing_the_old_socket() {
        let admission = Arc::new(Admission::default());
        let old = admission
            .socket(Some(ip(1)), SocketClass::Reservation)
            .unwrap();
        let next = admission.socket(Some(ip(2)), SocketClass::Initial).unwrap();
        let mut ticket = next.handoff(old.id()).unwrap();
        drop(next);
        assert_eq!(ticket.closed().await, Err(Denied::HandoffUnavailable));
        assert_eq!(admission.snapshot().promotions, 0);
        assert_eq!(admission.snapshot().sources[&ip(2)].held_long, 0);
        assert!(admission.lock().slots.contains_key(&old.id()));
        drop(old);
        drop(ticket);
        assert_eq!(
            (admission.snapshot().held_long, admission.snapshot().total),
            (0, 0)
        );
    }

    #[test]
    fn initial_source_denials_do_not_spend_global_requests_or_erase_rate_on_drop() {
        let admission = Arc::new(Admission::default());
        let mut sockets = Vec::new();
        for _ in 0..4 {
            sockets.push(admission.socket(Some(ip(1)), SocketClass::Initial).unwrap());
        }
        for _ in 0..200 {
            assert_eq!(
                admission
                    .socket(Some(ip(1)), SocketClass::Initial)
                    .unwrap_err(),
                Denied::SourceInitialSocket
            );
        }
        assert_eq!(admission.snapshot().inbound_requests, 4);
        for source in 2..=4 {
            for _ in 0..4 {
                sockets.push(
                    admission
                        .socket(Some(ip(source)), SocketClass::Initial)
                        .unwrap(),
                );
            }
        }
        assert_eq!(
            admission
                .socket(Some(ip(5)), SocketClass::Initial)
                .unwrap_err(),
            Denied::InitialSocket
        );
        let snapshot = admission.snapshot();
        assert_eq!((snapshot.total, snapshot.short, snapshot.long), (16, 16, 0));
        drop(sockets);
        assert_eq!(admission.snapshot().total, 0);
        assert_eq!(
            admission.snapshot().sources[&ip(1)],
            SourceSnapshot {
                short: 0,
                long: 0,
                held_long: 0,
                circuits: 0,
                requests: 4
            }
        );
    }

    #[test]
    fn outbound_entities_keep_control_capacity_and_total_is_bounded() {
        let admission = Arc::new(Admission::default());
        let mut data = Vec::new();
        for _ in 0..MAX_ESTABLISHED {
            data.push(admission.socket(None, SocketClass::Data).unwrap());
        }
        assert_eq!(
            admission
                .socket(None, SocketClass::Reservation)
                .unwrap_err(),
            Denied::EstablishedSocket
        );
        let mut control = Vec::new();
        for _ in 0..MAX_INITIAL {
            control.push(admission.socket(None, SocketClass::Control).unwrap());
        }
        assert_eq!(admission.snapshot().total, MAX_SOCKETS);
        assert_eq!(
            admission.socket(None, SocketClass::Control).unwrap_err(),
            Denied::GlobalSocket
        );
        control.pop();
        assert_eq!(
            admission.socket(None, SocketClass::Data).unwrap_err(),
            Denied::EstablishedSocket
        );
        drop(control);
        drop(data);
        let snapshot = admission.snapshot();
        assert_eq!((snapshot.total, snapshot.short, snapshot.long), (0, 0, 0));
        assert_eq!(snapshot.inbound_requests, 0);
    }

    #[test]
    fn failed_promotion_preserves_initial_and_release_allows_same_socket_to_promote() {
        let admission = Arc::new(Admission::default());
        let mut sockets = Vec::new();
        for _ in 0..16 {
            let mut socket = admission.socket(Some(ip(1)), SocketClass::Initial).unwrap();
            socket.promote(SocketClass::Data).unwrap();
            sockets.push(socket);
        }
        let mut next = admission.socket(Some(ip(1)), SocketClass::Initial).unwrap();
        assert_eq!(next.source(), Some(ip(1)));
        assert_eq!(
            next.promote(SocketClass::Reservation).unwrap_err(),
            Denied::SourceEstablishedSocket
        );
        assert_eq!(
            (admission.snapshot().short, admission.snapshot().long),
            (1, 16)
        );
        sockets.pop();
        next.promote(SocketClass::Reservation).unwrap();
        assert_eq!(
            (admission.snapshot().short, admission.snapshot().long),
            (0, 16)
        );
        drop(next);
        drop(sockets);
        assert_eq!(admission.snapshot().total, 0);
    }

    #[test]
    fn source_rate_precedes_global_rate_and_windows_reset_at_the_boundary() {
        let admission = Arc::new(Admission::default());
        for source in 1..=5 {
            for _ in 0..SOURCE_REQUESTS {
                drop(
                    admission
                        .socket(Some(ip(source)), SocketClass::Initial)
                        .unwrap(),
                );
            }
            assert_eq!(
                admission
                    .socket(Some(ip(source)), SocketClass::Initial)
                    .unwrap_err(),
                Denied::SourceRate
            );
            assert_eq!(
                admission.snapshot().inbound_requests,
                usize::from(source) * SOURCE_REQUESTS
            );
        }
        assert_eq!(
            admission
                .socket(Some(ip(6)), SocketClass::Initial)
                .unwrap_err(),
            Denied::GlobalRate
        );
        assert_eq!(admission.snapshot().sources.len(), 5);
        let now = Instant::now();
        let mut rate = RequestRate::default();
        for _ in 0..SOURCE_REQUESTS {
            rate.record(now);
        }
        assert!(!rate.available(
            now + REQUEST_WINDOW - Duration::from_nanos(1),
            SOURCE_REQUESTS
        ));
        assert!(rate.available(now + REQUEST_WINDOW, SOURCE_REQUESTS));
        assert_eq!(rate.count, 0);
    }

    #[test]
    fn waiting_circuits_share_source_target_and_global_limits_and_drop_prunes_keys() {
        let admission = Arc::new(Admission::default());
        let mut circuits = Vec::new();
        for _ in 0..4 {
            circuits.push(admission.circuit(ip(1), "first").unwrap());
        }
        assert_eq!(
            admission.circuit(ip(1), "other").unwrap_err(),
            Denied::SourceCircuit
        );
        assert_eq!(
            admission.circuit(ip(2), "first").unwrap_err(),
            Denied::TargetCircuit
        );
        for group in 2..=8 {
            for _ in 0..4 {
                circuits.push(
                    admission
                        .circuit(ip(group), &format!("target-{group}"))
                        .unwrap(),
                );
            }
        }
        assert_eq!(admission.snapshot().circuits, MAX_CIRCUITS);
        assert_eq!(
            admission.circuit(ip(9), "new").unwrap_err(),
            Denied::GlobalCircuit
        );
        drop(circuits);
        let snapshot = admission.snapshot();
        assert_eq!(snapshot.circuits, 0);
        assert!(snapshot.sources.is_empty());
        assert!(snapshot.targets.is_empty());
    }

    #[test]
    fn source_table_rejects_new_sources_without_evicting_active_or_recent_entries() {
        let admission = Arc::new(Admission::default());
        let short = admission.socket(Some(ip(1)), SocketClass::Initial).unwrap();
        let long = admission.socket(Some(ip(2)), SocketClass::Data).unwrap();
        let circuit = admission.circuit(ip(3), "active").unwrap();
        let now = Instant::now();
        {
            let mut state = admission.lock();
            for index in 0..MAX_SOURCES - 3 {
                let mut source = Source::default();
                source.requests.record(now);
                state.sources.insert(
                    IpAddr::from([10, (index >> 8) as u8, index as u8, 1]),
                    source,
                );
            }
        }
        assert_eq!(
            admission
                .socket(Some(ip(4)), SocketClass::Initial)
                .unwrap_err(),
            Denied::SourceTable
        );
        assert_eq!(admission.snapshot().sources.len(), MAX_SOURCES);
        for source in admission.lock().sources.values_mut() {
            if source.short == 0 && source.long == 0 && source.circuits == 0 {
                source.requests.start = Some(now - REQUEST_WINDOW);
            }
        }
        drop(admission.socket(Some(ip(4)), SocketClass::Initial).unwrap());
        assert_eq!(admission.snapshot().sources.len(), 4);
        admission.lock().prune(Instant::now() + REQUEST_WINDOW);
        let sources = admission.snapshot().sources;
        assert_eq!(sources.len(), 3);
        assert_eq!(sources[&ip(1)].short, 1);
        assert_eq!(sources[&ip(2)].long, 1);
        assert_eq!(sources[&ip(3)].circuits, 1);
        drop(short);
        drop(long);
        drop(circuit);
        admission.lock().prune(Instant::now() + REQUEST_WINDOW);
        assert!(admission.snapshot().sources.is_empty());
        assert_eq!(admission.snapshot().total, 0);
        assert!(!Denied::SourceInitialSocket.rate());
        assert!(Denied::SourceRate.rate());
    }
}
