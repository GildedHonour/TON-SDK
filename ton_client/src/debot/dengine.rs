use super::action::{AcType, DAction};
use super::browser::BrowserCallbacks;
use super::context::{
    str_hex_to_utf8, DContext, STATE_CURRENT, STATE_EXIT, STATE_PREV, STATE_ZERO,
};
use super::debot_abi::DEBOT_ABI;
use super::routines;
use crate::abi::{
    decode_message_body, encode_message, Abi, AbiConfig, CallSet, DeploySet,
    ParamsOfDecodeMessageBody, ParamsOfEncodeMessage, Signer,
};
use crate::crypto::{remove_signing_box, CryptoConfig, RegisteredSigningBox, SigningBoxHandle};
use crate::encoding::decode_abi_number;
use crate::error::ClientError;
use crate::net::{query_collection, NetworkConfig, ParamsOfQueryCollection};
use crate::processing::{process_message, ParamsOfProcessMessage, ProcessingEvent};
use crate::tvm::{run_tvm, ParamsOfRunTvm};
use crate::{ClientConfig, ClientContext};
use std::collections::VecDeque;
use std::sync::Arc;

pub type TonClient = Arc<ClientContext>;
type JsonValue = serde_json::Value;

fn create_client(url: &str) -> Result<TonClient, String> {
    let cli_conf = ClientConfig {
        abi: AbiConfig::default(),
        crypto: CryptoConfig::default(),
        network: NetworkConfig {
            server_address: url.to_owned(),
            ..Default::default()
        },
    };
    let cli =
        ClientContext::new(cli_conf).map_err(|e| format!("failed to create tonclient: {}", e))?;
    Ok(Arc::new(cli))
}

fn load_abi(abi: &str) -> Result<Abi, String> {
    Ok(Abi::Contract(
        serde_json::from_str(abi).map_err(|e| format!("failed to parse abi: {}", e))?,
    ))
}

struct RunOutput {
    output: Option<JsonValue>,
    #[allow(dead_code)]
    msgs: Vec<String>,
    account: String,
}

impl RunOutput {
    pub fn new(account: String, msgs: Vec<String>, output: Option<JsonValue>) -> Self {
        RunOutput {
            account,
            msgs,
            output,
        }
    }
}

// TODO: implement address validation
pub fn load_ton_address(addr: &str) -> Result<String, String> {
    Ok(addr.to_owned())
}

const OPTION_ABI: u8 = 1;
const OPTION_TARGET_ABI: u8 = 2;
const OPTION_TARGET_ADDR: u8 = 4;

/// Debot Engine.
/// Downloads and stores debot, executes its actions and calls 
/// Debot Browser callbacks.
pub struct DEngine {
    abi: Abi,
    addr: String,
    ton: TonClient,
    state: String,
    state_machine: Vec<DContext>,
    curr_state: u8,
    prev_state: u8,
    target_addr: Option<String>,
    target_abi: Option<String>,
    browser: Arc<dyn BrowserCallbacks + Send + Sync>,
}

impl DEngine {
    pub fn new(
        addr: String,
        abi: Option<String>,
        url: &str,
        browser: Arc<dyn BrowserCallbacks + Send + Sync>,
    ) -> Self {
        DEngine::new_with_client(addr, abi, create_client(url).unwrap(), browser)
    }

    pub fn new_with_client(
        addr: String,
        abi: Option<String>,
        ton: TonClient,
        browser: Arc<dyn BrowserCallbacks + Send + Sync>,
    ) -> Self {
        DEngine {
            abi: abi
                .map(|s| load_abi(&s))
                .unwrap_or(load_abi(DEBOT_ABI))
                .unwrap(),
            addr,
            ton,
            state: String::new(),
            state_machine: vec![],
            curr_state: STATE_EXIT,
            prev_state: STATE_ZERO,
            target_addr: None,
            target_abi: None,
            browser: browser,
        }
    }

    pub async fn fetch(&mut self) -> Result<(), String> {
        self.state_machine = self.fetch_state().await?;
        Ok(())
    }

    async fn fetch_state(&mut self) -> Result<Vec<DContext>, String> {
        self.state = self.load_state(self.addr.clone()).await?;
        let result = self.run_debot_get("getVersion", None).await?;

        let name_hex = result["name"].as_str().unwrap();
        let ver_str = result["semver"].as_str().unwrap();
        let name = str_hex_to_utf8(name_hex).unwrap();
        let ver = decode_abi_number::<u32>(ver_str).unwrap();
        self.browser.log(format!(
            "{}, version {}.{}.{}",
            name,
            (ver >> 16) as u8,
            (ver >> 8) as u8,
            ver as u8
        )).await;

        self.update_options().await?;
        let mut result = self.run_debot_get("fetch", None).await?;
        let context_vec: Vec<DContext> = serde_json::from_value(result["contexts"].take()).unwrap();
        Ok(context_vec)
    }

    pub async fn start(&mut self) -> Result<(), String> {
        self.state_machine = self.fetch_state().await?;
        self.switch_state(STATE_ZERO).await
    }

    #[allow(dead_code)]
    pub async fn version(&mut self) -> Result<String, String> {
        self.run_debot_get("getVersion", None)
            .await
            .map(|res| res.to_string())
    }

    pub async fn execute_action(&mut self, act: &DAction) -> Result<(), String> {
        match self.handle_action(&act).await {
            Ok(_) => self.switch_state(act.to).await,
            Err(e) => {
                self.browser
                    .log(format!("Action failed: {}. Return to previous state.\n", e))
                    .await;
                self.switch_state(self.prev_state).await
            }
        }
    }

    async fn handle_action(&mut self, a: &DAction) -> Result<Option<Vec<DAction>>, String> {
        match a.action_type {
            AcType::Empty => {
                debug!("empty action: {}", a.name);
                Ok(None)
            }
            AcType::RunAction => {
                debug!("run_action: {}", a.name);
                self.run_action(&a).await
            }
            AcType::RunMethod => {
                debug!("run_getmethod: {}", a.func_attr().unwrap());
                let args: Option<JsonValue> = if let Some(getter) = a.args_attr() {
                    self.run_debot(&getter, None).await?
                } else {
                    None
                };
                self.run_getmethod(&a.func_attr().unwrap(), args, &a.name)
                    .await?;
                Ok(None)
            }
            AcType::SendMsg => {
                debug!("sendmsg: {}", a.name);
                let signer = if a.sign_by_user() {
                    Some(self.browser.get_signing_box().await?)
                } else {
                    None
                };
                let args: Option<JsonValue> = if a.misc != /*empty cell*/"te6ccgEBAQEAAgAAAA==" {
                    Some(json!({ "misc": a.misc }).into())
                } else {
                    None
                };
                let result = self.run_sendmsg(&a.name, args, signer.clone()).await?;
                if let Some(signing_box) = signer {
                    let _ = remove_signing_box(
                        self.ton.clone(),
                        RegisteredSigningBox {
                            handle: signing_box
                    });
                }
                self.browser.log(format!("Transaction succeeded.")).await;
                result.map(|r| self.browser.log(format!("Result: {}", r)));
                Ok(None)
            }
            AcType::Invoke => {
                debug!("invoke debot: run {}", a.name);
                let result = self.run_debot(&a.name, None).await?;
                let invoke_args = result.ok_or(format!(
                    r#"invalid invoke action "{}": it must return "debot" and "action" arguments"#,
                    a.name
                ))?;
                debug!("{}", invoke_args);
                let debot_addr = load_ton_address(invoke_args["debot"].as_str().unwrap())?;
                let debot_action: DAction =
                    serde_json::from_value(invoke_args["action"].clone()).unwrap();
                debug!(
                    "invoke debot: {}, action name: {}",
                    &debot_addr, debot_action.name
                );
                self.browser.invoke_debot(debot_addr, debot_action).await?;
                Ok(None)
            }
            AcType::Print => {
                debug!("print action: {}", a.name);
                let label = if let Some(args_getter) = a.format_args() {
                    let args = if a.misc != /*empty cell*/"te6ccgEBAQEAAgAAAA==" {
                        Some(json!({"misc": a.misc}).into())
                    } else {
                        None
                    };
                    self.run_debot(&args_getter, args)
                        .await?
                        .map(|p| routines::format_string(&a.name, &p))
                        .unwrap_or_default()
                } else {
                    a.name.clone()
                };
                self.browser.log(label).await;
                Ok(None)
            }
            AcType::Goto => {
                debug!("goto action");
                Ok(None)
            }
            AcType::CallEngine => {
                debug!("call engine action: {}", a.name);
                let args = if let Some(args_getter) = a.args_attr() {
                    let args = self.run_debot(&args_getter, None).await?;
                    args.map(|v| v.to_string()).unwrap_or_default()
                } else {
                    a.desc.clone()
                };
                let signer = if a.sign_by_user() {
                    Some(self.browser.get_signing_box().await?)
                } else {
                    None
                };
                let res = self.call_routine(&a.name, &args, signer.clone()).await?;
                if let Some(signing_box) = signer {
                    let _ = remove_signing_box(
                        self.ton.clone(),
                        RegisteredSigningBox {
                            handle: signing_box
                    });
                }
                let setter = a
                    .func_attr()
                    .ok_or("routine callback is not specified".to_owned())?;
                self.run_debot(&setter, Some(json!({ "arg1": res }).into()))
                    .await?;
                Ok(None)
            }
            _ => {
                let err_msg = "unsupported action type".to_owned();
                self.browser.log(err_msg.clone()).await;
                Err(err_msg)
            }
        }
    }

    async fn switch_state(&mut self, mut state_to: u8) -> Result<(), String> {
        debug!("switching to {}", state_to);
        if state_to == STATE_CURRENT {
            state_to = self.curr_state;
        }
        if state_to == STATE_PREV {
            state_to = self.prev_state;
        }
        if state_to == STATE_EXIT {
            self.browser.switch(STATE_EXIT).await;
        } else if state_to != self.curr_state {
            let mut instant_switch = true;
            self.prev_state = self.curr_state;
            self.curr_state = state_to;
            while instant_switch {
                // TODO: restrict cyclic switches
                let jump_to_ctx = self
                    .state_machine
                    .iter()
                    .find(|ctx| ctx.id == state_to)
                    .map(|ctx| ctx.clone());
                if let Some(ctx) = jump_to_ctx {
                    self.browser.switch(state_to).await;
                    self.browser.log(ctx.desc.clone()).await;
                    instant_switch = self.enumerate_actions(ctx).await?;
                    state_to = self.curr_state;
                } else if state_to == STATE_EXIT {
                    self.browser.switch(STATE_EXIT).await;
                    instant_switch = false;
                } else {
                    self.browser
                        .log(format!("Debot context #{} not found. Exit.", state_to))
                        .await;
                    instant_switch = false;
                }
                debug!(
                    "instant_switch = {}, state_to = {}",
                    instant_switch, state_to
                );
            }
        }
        Ok(())
    }

    async fn enumerate_actions(&mut self, ctx: DContext) -> Result<bool, String> {
        // find, execute and remove instant action from context.
        // if instant action returns new actions then execute them and insert into context.
        for action in &ctx.actions {
            let mut sub_actions = VecDeque::new();
            sub_actions.push_back(action.clone());
            while let Some(act) = sub_actions.pop_front() {
                if act.is_instant() {
                    if act.desc.len() != 0 {
                        self.browser.log(act.desc.clone()).await;
                    }
                    self.handle_action(&act).await?.and_then(|vec| {
                        vec.iter().for_each(|a| sub_actions.push_back(a.clone()));
                        Some(())
                    });
                    // if instant action wants to switch context then exit and do switch.
                    let to = if act.to == STATE_CURRENT {
                        self.curr_state
                    } else if act.to == STATE_PREV {
                        self.prev_state
                    } else {
                        act.to
                    };
                    if to != self.curr_state {
                        self.curr_state = act.to;
                        return Ok(true);
                    }
                } else if act.is_engine_call() {
                    self.handle_action(&act).await?;
                } else {
                    self.browser.show_action(act).await;
                }
            }
        }
        Ok(false)
    }

    async fn run_debot_get(
        &self,
        func: &str,
        args: Option<JsonValue>,
    ) -> Result<JsonValue, String> {
        self.run(
            self.state.clone(),
            self.addr.clone(),
            self.abi.clone(),
            func,
            args,
        )
        .await
        .map(|res| res.output.unwrap_or(json!({})))
        .map_err(|e| format!("{}", e))
    }

    async fn run_get(
        &self,
        addr: String,
        abi: Abi,
        name: &str,
        params: Option<JsonValue>,
    ) -> Result<JsonValue, String> {
        let state = self.load_state(addr.clone()).await?;
        match self.run(state, addr, abi, name, params).await {
            Ok(res) => Ok(res.output.unwrap_or(json!({}))),
            Err(e) => Err(self.handle_sdk_err(e).await),
        }
    }

    async fn run_debot(
        &mut self,
        name: &str,
        args: Option<JsonValue>,
    ) -> Result<Option<JsonValue>, String> {
        debug!(
            "run_debot {}, args: {}",
            name,
            if args.is_some() {
                args.clone().unwrap()
            } else {
                json!({}).into()
            }
        );
        match self
            .run(
                self.state.clone(),
                self.addr.clone(),
                self.abi.clone(),
                name,
                args,
            )
            .await
        {
            Ok(res) => {
                self.state = res.account;
                Ok(res.output)
            }
            Err(e) => Err(self.handle_sdk_err(e).await),
        }
    }

    async fn run_action(&mut self, action: &DAction) -> Result<Option<Vec<DAction>>, String> {
        let args = self.query_action_args(action).await?;

        let output = self.run_debot(&action.name, args).await?;

        let action_vec: Option<Vec<DAction>> = output
            .map(|mut out| serde_json::from_value(out["actions"].take()))
            .transpose()
            .map_err(|_| format!("internal error: failed to parse actions"))?;
        Ok(action_vec)
    }

    async fn run_sendmsg(
        &mut self,
        name: &str,
        args: Option<JsonValue>,
        signer: Option<SigningBoxHandle>,
    ) -> Result<Option<JsonValue>, String> {
        let result = self.run_debot(name, args).await?;
        if result.is_none() {
            return Err(format!(
                r#"action "{}" is invalid: it must return "dest" and "body" arguments"#,
                name
            ));
        }
        let result = result.unwrap();
        let dest = result["dest"].as_str().unwrap();
        let body = result["body"].as_str().unwrap();
        let state = result["state"].as_str();

        let call_itself = load_ton_address(dest)? == self.addr;
        let abi = if call_itself {
            self.abi.clone()
        } else {
            let (_, abi) = self.get_target()?;
            abi
        };

        let res = decode_message_body(
            self.ton.clone(),
            ParamsOfDecodeMessageBody {
                abi: abi.clone(),
                body: body.to_string(),
                is_internal: true,
            },
        )
        .map_err(|e| format!("failed to decode msg body: {}", e))?;

        debug!("calling {} at address {}", res.name, dest);
        debug!("args: {}", res.value.as_ref().unwrap_or(&json!({})));
        self.call_target(dest, abi, &res.name, res.value.clone(), signer, state)
            .await
    }

    async fn run_getmethod(
        &mut self,
        getmethod: &str,
        args: Option<JsonValue>,
        result_handler: &str,
    ) -> Result<Option<JsonValue>, String> {
        self.update_options().await?;
        if self.target_addr.is_none() {
            return Err(format!("target address is undefined"));
        }
        let (addr, abi) = self.get_target()?;
        let result = self.run_get(addr, abi, getmethod, args).await?;
        self.run_debot(result_handler, Some(result)).await
    }

    async fn load_state(&self, addr: String) -> Result<String, String> {
        let account_request = query_collection(
            self.ton.clone(),
            ParamsOfQueryCollection {
                collection: "accounts".to_owned(),
                filter: Some(serde_json::json!({
                    "id": { "eq": addr }
                })),
                result: "boc".to_owned(),
                limit: Some(1),
                order: None,
            },
        )
        .await;
        let acc = account_request.map_err(|e| format!("failed to query debot account: {}", e))?;
        if acc.result.is_empty() {
            return Err(format!(
                "Cannot find debot with this address {} in blockchain",
                addr
            ));
        }
        let state = acc.result[0]["boc"].as_str().unwrap().to_owned();
        Ok(state)
    }

    async fn update_options(&mut self) -> Result<(), String> {
        let params = self.run_debot_get("getDebotOptions", None).await?;
        let opt_str = params["options"].as_str().unwrap();
        let options = decode_abi_number::<u8>(opt_str).unwrap();
        if options & OPTION_ABI != 0 {
            let abi_str = str_hex_to_utf8(params["debotAbi"].as_str().unwrap())
                .ok_or("cannot convert hex string to debot abi")?;
            self.abi = load_abi(&abi_str)?;
        }
        if options & OPTION_TARGET_ABI != 0 {
            self.target_abi = str_hex_to_utf8(params["targetAbi"].as_str().unwrap());
        }
        if (options & OPTION_TARGET_ADDR) != 0 {
            let addr = params["targetAddr"].as_str().unwrap();
            self.target_addr = Some(load_ton_address(addr)?);
        }
        Ok(())
    }

    async fn query_action_args(&self, act: &DAction) -> Result<Option<JsonValue>, String> {
        let args: Option<JsonValue> = if act.misc != /*empty cell*/"te6ccgEBAQEAAgAAAA==" {
            Some(json!({ "misc": act.misc }).into())
        } else {
            let abi_json: serde_json::Value = if let Abi::Contract(ref abi_obj) = self.abi {
                serde_json::from_str(&serde_json::to_string(&abi_obj).unwrap()).unwrap()
            } else {
                json!({})
            };
            let functions = abi_json["functions"].as_array().unwrap();
            let func = functions
                .iter()
                .find(|f| f["name"].as_str().unwrap() == act.name)
                .ok_or(format!("action not found"))?;
            let arguments = func["inputs"].as_array().unwrap();
            let mut args_json = json!({});
            for arg in arguments {
                let arg_name = arg["name"].as_str().unwrap();
                let prompt = "".to_owned();
                let mut value = String::new();
                self.browser.input(&prompt, &mut value).await;
                if arg["type"].as_str().unwrap() == "bytes" {
                    value = hex::encode(value.as_bytes());
                }
                args_json[arg_name] = json!(&value);
            }
            Some(args_json.into())
        };
        Ok(args)
    }

    fn get_target(&self) -> Result<(String, Abi), String> {
        let addr = self
            .target_addr
            .clone()
            .ok_or(format!("target address is undefined"))?;
        let abi = self
            .target_abi
            .as_ref()
            .ok_or(format!("target abi is undefined"))?;
        let abi_obj = load_abi(abi)?;
        Ok((addr, abi_obj))
    }

    async fn run(
        &self,
        state: String,
        addr: String,
        abi: Abi,
        func: &str,
        args: Option<JsonValue>,
    ) -> Result<RunOutput, ClientError> {
        debug!("running {}, addr {}", func, &addr);

        let msg_params = ParamsOfEncodeMessage {
            abi: abi.clone(),
            address: Some(addr),
            deploy_set: None,
            call_set: if args.is_none() {
                CallSet::some_with_function(func)
            } else {
                CallSet::some_with_function_and_input(func, args.unwrap())
            },
            signer: Signer::None,
            processing_try_index: None,
        };

        let result = encode_message(self.ton.clone(), msg_params).await?;

        match run_tvm(
            self.ton.clone(),
            ParamsOfRunTvm {
                account: state,
                message: result.message,
                abi: Some(abi),
                execution_options: None,
            },
        )
        .await
        {
            Ok(res) => Ok(RunOutput::new(
                res.account,
                res.out_messages,
                res.decoded.unwrap().output,
            )),
            Err(e) => {
                error!("{}", e);
                Err(e)
            }
        }
    }

    async fn call_target(
        &self,
        dest: &str,
        abi: Abi,
        func: &str,
        args: Option<JsonValue>,
        signer: Option<SigningBoxHandle>,
        state: Option<&str>,
    ) -> Result<Option<JsonValue>, String> {
        let addr = load_ton_address(dest)?;

        let call_params = ParamsOfEncodeMessage {
            abi: abi.clone(),
            address: Some(addr),
            deploy_set: state.and_then(|s| DeploySet::some_with_tvc(s.to_string())),
            call_set: if args.is_none() {
                CallSet::some_with_function(func)
            } else {
                CallSet::some_with_function_and_input(func, args.unwrap())
            },
            signer: match signer {
                Some(signing_box) => Signer::SigningBox { handle: signing_box },
                None => Signer::None,
            },
            processing_try_index: None,
        };

        //let msg = pack_state(msg, state)?;
        let browser = self.browser.clone();
        let callback = move |event| {
            debug!("{:?}", event);
            let browser = browser.clone();
            async move {
                match event {
                    ProcessingEvent::WillSend { shard_block_id: _, message_id, message: _ } => {
                        browser.log(format!("Sending message {}", message_id)).await;
                    },
                    _ => (),
                }; 
            }
        };
        
        match process_message(
            self.ton.clone(),
            ParamsOfProcessMessage {
                message_encode_params: call_params,
                send_events: true,
            },
            callback,
        )
        .await
        {
            Ok(res) => Ok(res.decoded.unwrap().output),
            Err(e) => {
                error!("{}", e);
                Err(self.handle_sdk_err(e).await)
            }
        }
    }

    async fn call_routine(
        &self,
        name: &str,
        args: &str,
        signer: Option<SigningBoxHandle>,
    ) -> Result<String, String> {
        routines::call_routine(self.ton.clone(), name, args, signer).await
    }

    async fn handle_sdk_err(&self, err: ClientError) -> String {
        if err.message.contains("Wrong data format") {
            // when debot's function argument has invalid format
            "invalid parameter".to_owned()
        } else if err.code == 3025 {
            // when debot function throws an exception
            if let Some(e) = err.data["exit_code"].as_i64() {
                self.run_debot_get("getErrorDescription", Some(json!({ "error": e })))
                    .await
                    .ok()
                    .and_then(|res| {
                        res["desc"].as_str().and_then(|hex| {
                            hex::decode(&hex)
                                .ok()
                                .and_then(|vec| String::from_utf8(vec).ok())
                        })
                    })
                    .unwrap_or(err.message)
            } else {
                err.message
            }
        } else {
            err.message
        }
    }
}