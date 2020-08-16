use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;

/// Host request
#[derive(Deserialize)]
pub struct Request;

/// Agent response
#[derive(Serialize)]
pub struct Response;

/// Message callback function
type MessageCallbackFn<Data> = fn(Request, Data) -> crate::error::Result<Response>;

/// Message callback function and parameter
struct MessageCallback<Data>
where
    Data: Clone + Send + Sync + 'static,
{
    pub function: MessageCallbackFn<Data>,
    pub parameter: Data,
}

/// Agent instance builder
pub struct AgentBuilder<D: Clone + Send + Sync + 'static> {
    /// Local agent name
    name: String,
    /// Host url, a string like "wss://example.com/ws/"
    host_addr: Option<String>,
    /// Callback structure, with callback function point and parameter.
    message_callback: Option<MessageCallback<D>>,
}

impl<D: Clone + Send + Sync + 'static> AgentBuilder<D> {
    /// Create a new agent instance.
    pub fn new(name: String) -> Self {
        Self {
            name,
            host_addr: None,
            message_callback: None,
        }
    }

    /// Set host address
    pub fn host(mut self, addr: &str) -> Self {
        self.host_addr = Some(addr.to_string());
        self
    }

    /// Set callback function which will be called when packet comes.
    pub fn set_callback(mut self, callback_fn: MessageCallbackFn<D>, parameter: D) -> Self {
        self.message_callback = Some(MessageCallback {
            function: callback_fn,
            parameter,
        });
        self
    }

    /// Build a valid Agent structure. `panic` if host or callback function is not set.
    pub fn build(self) -> Agent<D> {
        Agent {
            name: self.name,
            host_addr: self.host_addr.expect("Host address is needed."),
            message_callback: Arc::new(
                self.message_callback.expect("You should set callback function."),
            ),
        }
    }
}

/// Agent node in campus side.
pub struct Agent<D>
where
    D: Clone + Send + Sync + 'static,
{
    /// Local agent name
    name: String,
    /// Host url, a string like "wss://example.com/ws/"
    host_addr: String,
    /// Callback structure, with callback function point and parameter.
    message_callback: Arc<MessageCallback<D>>,
}

impl<D> Agent<D>
where
    D: Clone + Send + Sync + 'static,
{
    /// Unpack binary request payload, do the command, then pack and send response to host.
    async fn dispatch_message(
        content: Vec<u8>,
        mut socket_tx: mpsc::Sender<Message>,
        on_message: Arc<MessageCallback<D>>,
    ) {
        let request = bincode::deserialize(&content);
        if let Ok(req) = request {
            // Get callback function pointer and parameter.
            let request_callback = on_message.function;
            let callback_parameter = on_message.parameter.clone();

            // TODO: Return result instead of doing nothing.
            // If callback functions successfully, serialize the response and send back to host.
            if let Ok(response) = request_callback(req, callback_parameter) {
                let response_content = bincode::serialize(&response);
                if let Ok(response_content) = response_content {
                    socket_tx.send(Message::Binary(response_content)).await;
                }
            }
        }
        // TODO: Send error code `unknown`.
    }

    /// Unpack WebSocket message, match types and respond correctly.
    async fn process_message(
        message: Message,
        mut message_tx: mpsc::Sender<Message>,
        on_message: Arc<MessageCallback<D>>,
    ) {
        // Resolve request message, and response.
        // For Ping, Pong, Close message, we can send response immediately, while for binary we need
        // to decode and usually do further operation then.
        match message {
            Message::Binary(content) => {
                // Spawn new thread to execute the function because it usually costs a lot of time.
                actix_rt::spawn(Self::dispatch_message(content, message_tx, on_message.clone()));
            }
            Message::Ping(_) => {
                // Pong will be responded automatically by the framework.
                ()
            }
            Message::Pong(_) => {
                // Do nothing if Pong received
                ()
            }
            _ => {
                // When Message::Close or Message::Text (which unexpected for us) received,
                // close connection.
                message_tx.send(Message::Close(None)).await;
            }
        }
    }

    /// Receiver loop, accept commands and requests from the host.
    async fn receiver_loop<T>(
        mut socket_rx: T,
        message_tx: mpsc::Sender<Message>,
        on_message: Arc<MessageCallback<D>>,
    ) where
        T: StreamExt + std::marker::Unpin,
        T::Item: Into<std::result::Result<Message, tokio_tungstenite::tungstenite::Error>>,
    {
        while let Some(r) = socket_rx.next().await {
            match r.into() {
                Ok(message) => {
                    Self::process_message(message, message_tx.clone(), on_message.clone()).await
                }
                Err(_) => {}
            }
        }
    }

    /// Send response to host.
    async fn sender_loop<T, Item>(mut socket_tx: T, mut message_rx: mpsc::Receiver<Message>)
    where
        T: SinkExt<Item> + std::marker::Unpin,
        Item: From<Message>,
    {
        while let Some(response) = message_rx.recv().await {
            socket_tx.send(response.into()).await;
        }
    }

    /// Connect to host and start necessary event loop for communication over WebSocket.
    pub async fn start(&mut self) {
        let (socket, _) = tokio_tungstenite::connect_async(&self.host_addr).await.unwrap();
        let (write, read) = socket.split();
        let (tx, rx) = mpsc::channel::<Message>(128);

        // Spawn receiver loop.
        tokio::spawn(Self::receiver_loop(read, tx, self.message_callback.clone()));
        // Spawn sender loop.
        tokio::spawn(Self::sender_loop(write, rx));
    }
}
