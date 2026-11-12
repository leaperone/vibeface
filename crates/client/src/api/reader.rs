use super::*;

/// Authenticated reads and event subscriptions without a durable writer lease.
#[derive(Clone)]
pub struct ClientReader {
    key: Arc<ClientKey>,
    connection: Connection,
    target: String,
    network: String,
    generation: String,
    events: broadcast::Sender<ClientEvent>,
    active: Arc<AtomicBool>,
}
impl ClientReader {
    pub async fn open(key: Arc<ClientKey>, endpoint: Endpoint) -> Result<Self> {
        let session = Client::authenticate(&key, &endpoint).await?;
        let network = session.proof.network_id;
        let connection = session.connection;
        let (events, generation) = subscribe_connection(&connection, &network).await?;
        Ok(Self {
            key,
            connection,
            target: endpoint.node_id().to_owned(),
            network,
            generation,
            events,
            active: Arc::new(AtomicBool::new(true)),
        })
    }
    pub async fn read(&self, request: ReadRequest) -> Result<Value> {
        ensure!(
            self.active.load(Ordering::Acquire),
            "reader generation is closed"
        );
        read_connection(&self.connection, request).await
    }
    pub fn subscribe(&self) -> broadcast::Receiver<ClientEvent> {
        self.events.subscribe()
    }
    pub async fn reconnect(&self, endpoint: Endpoint) -> Result<Self> {
        let next = Self::open(self.key.clone(), endpoint).await?;
        ensure!(
            next.network == self.network,
            "reconnect cannot switch personal networks"
        );
        self.close();
        Ok(next)
    }
    pub fn close(&self) {
        self.active.store(false, Ordering::Release);
        self.connection.close();
    }
    pub fn network_id(&self) -> &str {
        &self.network
    }
    pub fn target_node(&self) -> &str {
        &self.target
    }
    pub fn generation(&self) -> &str {
        &self.generation
    }
    pub fn client_id(&self) -> &str {
        self.key.device_id()
    }
}
