use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{WebSocketStream, tungstenite::Message};
use vibeface_client::Entry;
use vibeface_secure::{
    Channel, Handshake, Identity,
    membership::{Genesis, IdentityKind, Member, NetworkState},
};

pub struct ControlledNode {
    pub listener: TcpListener,
    pub identity: Identity,
    pub state: NetworkState,
    pub entry: Entry,
}
impl ControlledNode {
    pub async fn new(client: &str, client_name: &str, node_name: &str) -> Result<Self> {
        let identity = Identity::generate()?;
        let state = NetworkState::new(Genesis::create(
            &Identity::generate()?,
            Member::new(client, IdentityKind::Client, client_name, true)?,
            Member::node(&identity, node_name)?,
        )?)?;
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let entry = Entry {
            origin: format!("http://{}", listener.local_addr()?),
            node_id: identity.device_id(),
        };
        Ok(Self {
            listener,
            identity,
            state,
            entry,
        })
    }
}

pub async fn accept_noise(
    socket: &mut WebSocketStream<TcpStream>,
    node: &Identity,
) -> Result<Channel> {
    let mut handshake = Handshake::signed(false, node, None, b"vibeface:client:v1")?;
    let Message::Binary(first) = socket.next().await.context("first Noise")?? else {
        anyhow::bail!("Noise frame missing")
    };
    handshake.read(&first)?;
    socket
        .send(Message::Binary(handshake.write(&[])?.into()))
        .await?;
    let Message::Binary(last) = socket.next().await.context("last Noise")?? else {
        anyhow::bail!("Noise frame missing")
    };
    handshake.read(&last)?;
    Ok(handshake.finish()?)
}
