/*
 * Copyright 2018-2020 TON DEV SOLUTIONS LTD.
 *
 * Licensed under the SOFTWARE EVALUATION License (the "License"); you may not use
 * this file except in compliance with the License.
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific TON DEV software governing permissions and
 * limitations under the License.
 *
 */

use crate::client::ClientEnv;
use crate::error::{ClientError, ClientResult};
use crate::net::NetworkConfig;
use futures::{FutureExt, SinkExt, StreamExt};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc::{channel, Receiver, Sender};

static GQL_CONNECTION_INIT: &str = "connection_init";
static GQL_CONNECTION_ACK: &str = "connection_ack";
static GQL_CONNECTION_ERROR: &str = "connection_error";
static GQL_CONNECTION_KEEP_ALIVE: &str = "ka";
static GQL_CONNECTION_TERMINATE: &str = "connection_terminate";
static GQL_START: &str = "start";
static GQL_DATA: &str = "data";
static GQL_ERROR: &str = "error";
static GQL_COMPLETE: &str = "complete";
static GQL_STOP: &str = "stop";

pub(crate) struct GqlQueryOperation {
    pub query: String,
    pub variables: Value,
}

pub enum GqlOperationEvent {
    Id(u32),
    Data(Value),
    Error(ClientError),
    Complete,
}

enum LinkAction {
    Suspend,
    Resume,
    Drop,
    None,
    StartOperation(Value, Sender<GqlOperationEvent>),
    StopOperation(u32),

    HandleServerMessage(ClientResult<String>),
    HandleServerError(ClientError),
}

//================================================================================== WebsocketLink

#[derive(Clone)]
pub(crate) struct WebsocketLink {
    action_sender: Sender<LinkAction>,
}

impl WebsocketLink {
    pub fn new(config: NetworkConfig, client_env: Arc<ClientEnv>) -> Self {
        let (action_sender, action_receiver) = channel(1);
        let handler_action_sender = action_sender.clone();
        client_env.clone().spawn(Box::pin(async move {
            let mut handler =
                LinkHandler::new(config, client_env, handler_action_sender, action_receiver);
            handler.run().await;
        }));
        Self { action_sender }
    }

    pub async fn start_operation(
        &mut self,
        request: GqlQueryOperation,
    ) -> ClientResult<Receiver<GqlOperationEvent>> {
        let (event_sender, event_receiver) = channel(1);
        let start_action = LinkAction::StartOperation(
            json!({
                "query": request.query,
                "variables": request.variables,
            }),
            event_sender,
        );
        self.send_action(start_action).await?;
        Ok(event_receiver)
    }

    pub async fn stop_operation(&mut self, id: u32) -> ClientResult<()> {
        self.send_action(LinkAction::StopOperation(id)).await
    }

    async fn send_action(&mut self, action: LinkAction) -> ClientResult<()> {
        self.action_sender
            .send(action)
            .await
            .map_err(|err| crate::client::Error::internal_error(&format!("{}", err)))
    }
}

//==================================================================================== LinkHandler

enum WebsocketAction {
    Drop,
    Send(String),
}

struct Operation {
    payload: Value,
    event_sender: Sender<GqlOperationEvent>,
}

struct LinkHandler {
    config: NetworkConfig,
    client_env: Arc<ClientEnv>,
    action_sender: Sender<LinkAction>,
    action_receiver: Receiver<LinkAction>,
    last_operation_id: u32,
    operations: HashMap<u32, Operation>,
    websocket_action_sender: Option<Sender<WebsocketAction>>,
}

impl LinkHandler {
    fn new(
        config: NetworkConfig,
        client_env: Arc<ClientEnv>,
        action_sender: Sender<LinkAction>,
        action_receiver: Receiver<LinkAction>,
    ) -> Self {
        Self {
            config,
            client_env,
            action_sender,
            action_receiver,
            last_operation_id: 0,
            operations: HashMap::new(),
            websocket_action_sender: None,
        }
    }

    async fn run(&mut self) {
        loop {
            let action = self
                .action_receiver
                .recv()
                .await
                .unwrap_or(LinkAction::None);
            match action {
                LinkAction::Suspend => self.suspend().await,
                LinkAction::Resume => self.resume().await,
                LinkAction::StartOperation(payload, event_sender) => {
                    self.start_operation(payload, event_sender).await;
                }
                LinkAction::StopOperation(id) => self.stop_operation(id).await,
                LinkAction::HandleServerMessage(message) => {
                    self.handle_server_message(message).await
                }
                LinkAction::HandleServerError(error) => self.handle_server_error(error).await,
                LinkAction::Drop => {
                    break;
                }
                LinkAction::None => {}
            }
        }
    }

    // Action handlers

    async fn suspend(&mut self) {}
    async fn resume(&mut self) {}

    async fn start_operation(&mut self, payload: Value, event_sender: Sender<GqlOperationEvent>) {
        let mut id = self.last_operation_id.wrapping_add(1);
        while id == 0 || self.operations.contains_key(&id) {
            id = id.wrapping_add(1);
        }
        let mut operation = Operation {
            payload: payload.clone(),
            event_sender,
        };
        operation.event_sender.send(GqlOperationEvent::Id(id)).await;
        self.operations.insert(id, operation);
        self.last_operation_id = id;
        self.send_websocket_data(GQL_START, Some(id), Some(payload))
            .await;
    }

    async fn stop_operation(&mut self, id: u32) {
        if let Some(mut op) = self.operations.remove(&id) {
            let _ = op.event_sender.send(GqlOperationEvent::Complete).await;
            self.send_websocket_data(GQL_STOP, Some(id), None).await;
        }
    }

    async fn handle_server_message(&mut self, _message: ClientResult<String>) {}

    async fn handle_server_error(&mut self, _error: ClientError) {}

    // Internals

    async fn ensure_websocket(&mut self) -> ClientResult<()> {
        if self.websocket_action_sender.is_some() {
            return Ok(());
        }
        // self.ensure_info().await?;
        // let client_lock = self.server_info.read().await;
        // client_lock.as_ref().unwrap().subscription_url

        let address = self.config.server_address.to_string();
        let mut headers = HashMap::new();
        headers.insert("Sec-WebSocket-Protocol".into(), "graphql-ws".into());
        let mut websocket = self
            .client_env
            .websocket_connect(&address, Some(headers))
            .await?;
        let (websocket_action_sender, mut websocket_action_receiver) = channel(1);
        self.websocket_action_sender = Some(websocket_action_sender);
        let mut action_sender = self.action_sender.clone();
        self.client_env.spawn(Box::pin(async move {
            let mut data_stream = websocket.receiver.fuse();
            let wait_action = websocket_action_receiver.recv().fuse();
            futures::pin_mut!(wait_action);
            loop {
                futures::select!(
                    data = data_stream.select_next_some() => {
                        action_sender.send(LinkAction::HandleServerMessage(data)).await;
                    },
                    action = wait_action => match action {
                        None => {
                            break;
                        },
                        Some(WebsocketAction::Drop) => { break },
                        Some(WebsocketAction::Send(data)) => { websocket.sender.send(data); }
                    }
                );
            }
        }));
        Ok(())
    }

    async fn send_websocket_data(
        &mut self,
        ty: &str,
        id: Option<u32>,
        payload: Option<Value>,
    ) -> ClientResult<()> {
        if let Some(ref mut websocket) = self.websocket_action_sender {
            let mut data = json!({ "type": ty });
            if let Some(id) = id {
                data["id"] = Value::from(id);
            }
            if let Some(payload) = payload {
                data["payload"] = payload;
            }
            websocket
                .send(WebsocketAction::Send(data.to_string()))
                .await
                .map_err(|err| {
                    crate::client::Error::internal_error(&format!(
                        "send_websocket_data failed: {}",
                        err
                    ))
                })
        } else {
            Ok(())
        }
    }
}
